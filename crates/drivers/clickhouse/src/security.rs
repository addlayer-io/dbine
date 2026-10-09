//! Users, roles and permissions (docs/users-and-permissions.md) through
//! ClickHouse's SQL access control (`system.users`, `system.roles`,
//! `system.role_grants`, `system.grants`), which Timeplus Proton shares.
//!
//! Users defined in the server's configuration files (`users.xml`) are
//! read-only from SQL: they're marked as system. ClickHouse has no account
//! lock; disabling a user means `HOST NONE` (it can't sign in from
//! anywhere), and enabling it `HOST ANY`.

use crate::schema::{q, string_literal as lit};
use crate::{text, ClickHouseSession, Flavor};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};

pub(crate) fn spec(flavor: Flavor) -> SecuritySpec {
    let mut object_kinds = vec!["", "schema", "table", "view", "materialized_view", "dictionary"];
    if flavor == Flavor::Timeplus {
        object_kinds.insert(2, "stream");
    }
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "ALTER", "CREATE", "DROP", "TRUNCATE", "OPTIMIZE", "SHOW", "dictGet", "ALTER UPDATE",
            "ALTER DELETE", "CREATE TABLE", "CREATE VIEW", "CREATE DICTIONARY", "DROP TABLE", "KILL QUERY", "SYSTEM",
            "INTROSPECTION", "SOURCES", "ACCESS MANAGEMENT", "ALL",
        ],
        object_kinds,
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

const USERS: &str = "SELECT name, storage, toString(auth_type), toString(host_ip), toString(host_names),
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
    let admins: HashSet<String> = s.rows(ADMINS, &[]).await.unwrap_or_default().iter().filter_map(|r| cell(r, 0)).collect();
    let mut out = Vec::new();
    for r in &users {
        let name = cell(r, 0).unwrap_or_default();
        let storage = cell(r, 1).unwrap_or_default();
        let empty = |i| matches!(cell(r, i).as_deref(), None | Some("[]"));
        let mut details = vec![("Almacenamiento".into(), storage.clone())];
        if let Some(a) = cell(r, 2).filter(|a| a != "[]") {
            details.push(("Autenticación".into(), a.trim_matches(|c| c == '[' || c == ']').replace('\'', "")));
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
        let (Some(member), Some(role)) = (cell(&r, 0).or_else(|| cell(&r, 1)), cell(&r, 2)) else { continue };
        if let Some(p) = out.iter_mut().find(|p| p.name == member) {
            p.member_of.push(role);
        }
    }
    Ok(out)
}

pub(crate) async fn grants(s: &mut ClickHouseSession, principal: &str) -> Result<Vec<Grant>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::from([(principal.to_string(), None)]);
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

/// Privilege names: letters, spaces and underscores, optionally with a
/// column list (`SELECT (a, b)`).
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let bad = |x: &str| Error::Query(format!("«{x}» no es un permiso de ClickHouse"));
    let mut out = Vec::new();
    for x in p {
        let (name, cols) = match x.split_once('(') {
            Some((n, c)) => (n.trim(), Some(c.trim().strip_suffix(')').ok_or_else(|| bad(x))?)),
            None => (x.trim(), None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '_') {
            return Err(bad(x));
        }
        match cols {
            Some(c) => {
                let cols: Vec<String> = c.split(',').map(|c| c.trim().trim_matches('`')).filter(|c| !c.is_empty()).map(q).collect();
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
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} IDENTIFIED WITH sha256_password BY {};", q(name), lit(pw))
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::SetPassword { name, password } => {
            format!("ALTER USER {} IDENTIFIED WITH sha256_password BY {};", q(name), lit(password))
        }
        SecurityAction::SetLogin { name, enabled: false } => {
            format!("-- ClickHouse no bloquea cuentas: sin hosts permitidos, no puede ingresar.\nALTER USER {} HOST NONE;", q(name))
        }
        SecurityAction::SetLogin { name, enabled: true } => {
            format!("-- Permite el ingreso desde cualquier host (reemplaza la lista anterior).\nALTER USER {} HOST ANY;", q(name))
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            format!("GRANT {} ON {} TO {}{};", privileges(p)?, on(object), q(to), if *grantable { " WITH GRANT OPTION" } else { "" })
        }
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {} ON {} FROM {};", privileges(p)?, on(object), q(from)),
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", q(role), q(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", q(role), q(member)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana`x".into(), password: Some("p'w".into()) }),
            "CREATE USER `ana\\`x` IDENTIFIED WITH sha256_password BY 'p\\'w';"
        );
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "lectores".into() }), "CREATE ROLE `lectores`;");
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
            s(SecurityAction::Revoke { privileges: vec!["dictGet".into()], object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "d".into() }), from: "r".into() }),
            "REVOKE dictGet ON `d`.* FROM `r`;"
        );
        assert_eq!(s(SecurityAction::Grant { privileges: vec!["SHOW".into()], object: None, to: "r".into(), grantable: false }), "GRANT SHOW ON *.* TO `r`;");
        assert!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }).ends_with("ALTER USER `ana` HOST NONE;"));
        assert_eq!(s(SecurityAction::AddMember { role: "r".into(), member: "ana".into() }), "GRANT `r` TO `ana`;");
        assert_eq!(s(SecurityAction::RemoveMember { role: "r".into(), member: "ana".into() }), "REVOKE `r` FROM `ana`;");
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT; DROP TABLE x".into()], object: None, to: "a".into(), grantable: false }).is_err());
    }
}

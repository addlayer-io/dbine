//! Users, roles and permissions (docs/users-and-permissions.md). Users live in
//! the security database (`SEC$USERS`, Firebird 3+); roles and privileges in
//! this database (`RDB$ROLES`, `RDB$USER_PRIVILEGES`). Without admin rights
//! `SEC$USERS` shows only the session's own user; the ones that only appear
//! as grantees (PUBLIC, users of another security database) are listed too.

use crate::{int, q, text, FirebirdSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use rsfbclient_core::Column;
use std::collections::{HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "EXECUTE", "USAGE", "ALL",
            // DDL privileges (Firebird 4+), granted without an object.
            "CREATE TABLE", "ALTER ANY TABLE", "DROP ANY TABLE", "CREATE VIEW", "CREATE PROCEDURE", "CREATE FUNCTION",
            "CREATE SEQUENCE",
        ],
        object_kinds: vec!["", "table", "view", "procedure", "function", "package", "sequence"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

const USERS: &str = "SELECT TRIM(SEC$USER_NAME), CASE WHEN SEC$ACTIVE IS FALSE THEN 0 ELSE 1 END,
       CASE WHEN SEC$ADMIN THEN 1 ELSE 0 END, TRIM(SEC$PLUGIN),
       TRIM(COALESCE(SEC$FIRST_NAME, '') || ' ' || COALESCE(SEC$LAST_NAME, ''))
  FROM SEC$USERS ORDER BY 1";
const GRANTEES: &str = "SELECT DISTINCT TRIM(RDB$USER) FROM RDB$USER_PRIVILEGES WHERE RDB$USER_TYPE = 8 ORDER BY 1";
const ROLES: &str = "SELECT TRIM(RDB$ROLE_NAME), COALESCE(RDB$SYSTEM_FLAG, 0), TRIM(RDB$OWNER_NAME) FROM RDB$ROLES ORDER BY 1";
const MEMBERS: &str = "SELECT TRIM(RDB$USER), TRIM(RDB$RELATION_NAME) FROM RDB$USER_PRIVILEGES WHERE RDB$PRIVILEGE = 'M'";
const ME: &str = "SELECT TRIM(CURRENT_USER) FROM RDB$DATABASE";

const GRANTS: &str = "SELECT TRIM(p.RDB$PRIVILEGE), p.RDB$GRANT_OPTION, TRIM(p.RDB$RELATION_NAME), TRIM(p.RDB$FIELD_NAME),
       p.RDB$OBJECT_TYPE,
       (SELECT CASE WHEN r.RDB$VIEW_BLR IS NULL THEN 0 ELSE 1 END FROM RDB$RELATIONS r
         WHERE r.RDB$RELATION_NAME = p.RDB$RELATION_NAME)
  FROM RDB$USER_PRIVILEGES p
 WHERE p.RDB$USER = ? AND p.RDB$USER_TYPE IN (8, 13) AND p.RDB$PRIVILEGE <> 'M'
 ORDER BY p.RDB$OBJECT_TYPE, 3, 1";
const ROLES_OF: &str = "SELECT TRIM(RDB$RELATION_NAME) FROM RDB$USER_PRIVILEGES
 WHERE RDB$USER = ? AND RDB$USER_TYPE IN (8, 13) AND RDB$PRIVILEGE = 'M'";

pub async fn principals(s: &FirebirdSession) -> Result<Vec<Principal>> {
    let users = s.rows(USERS, vec![]).await?;
    let grantees = s.rows(GRANTEES, vec![]).await?;
    let roles = s.rows(ROLES, vec![]).await?;
    let members = s.rows(MEMBERS, vec![]).await?;
    let me = s.rows(ME, vec![]).await?.first().and_then(|r| r.first()).and_then(text).unwrap_or_default();
    let col = |r: &Vec<Column>, i: usize| r.get(i).and_then(text).filter(|s| !s.is_empty());
    let flag = |r: &Vec<Column>, i: usize| r.get(i).and_then(int).unwrap_or(0) != 0;

    let mut out = Vec::new();
    let only_me = users.len() == 1 && col(&users[0], 0).as_deref() == Some(me.as_str()) && !flag(&users[0], 2);
    for r in &users {
        let name = col(r, 0).unwrap_or_default();
        let mut details = Vec::new();
        if let Some(p) = col(r, 3) {
            details.push(("Autenticación".into(), p));
        }
        if let Some(n) = col(r, 4) {
            details.push(("Nombre".into(), n));
        }
        if only_me {
            details.push(("Nota".into(), "Sin permisos de administrador: de la base de seguridad se ve solo el usuario propio".into()));
        }
        out.push(Principal {
            kind: PrincipalKind::User,
            can_login: Some(true),
            superuser: Some(flag(r, 2) || name == "SYSDBA"),
            disabled: Some(!flag(r, 1)),
            member_of: Vec::new(),
            details,
            system: name == "SYSDBA",
            name,
        });
    }
    for r in &grantees {
        let Some(name) = col(r, 0) else { continue };
        if out.iter().any(|p| p.name == name) {
            continue;
        }
        let public = name == "PUBLIC";
        out.push(Principal {
            kind: PrincipalKind::User,
            details: if public { Vec::new() } else { vec![("Cuenta".into(), "no figura en la base de seguridad".into())] },
            system: public,
            name,
            ..Default::default()
        });
    }
    for r in &roles {
        let name = col(r, 0).unwrap_or_default();
        let mut details = Vec::new();
        if let Some(o) = col(r, 2) {
            details.push(("Dueño".into(), o));
        }
        out.push(Principal {
            kind: PrincipalKind::Role,
            superuser: Some(name == "RDB$ADMIN"),
            system: flag(r, 1),
            details,
            name,
            ..Default::default()
        });
    }
    for m in &members {
        let (Some(member), Some(role)) = (col(m, 0), col(m, 1)) else { continue };
        if let Some(p) = out.iter_mut().find(|p| p.name == member) {
            if !p.member_of.contains(&role) {
                if role == "RDB$ADMIN" {
                    p.superuser = Some(true);
                }
                p.member_of.push(role);
            }
        }
    }
    Ok(out)
}

/// `RDB$PRIVILEGE` as SQL.
fn verb(code: &str) -> &str {
    match code {
        "S" => "SELECT",
        "I" => "INSERT",
        "U" => "UPDATE",
        "D" => "DELETE",
        "R" => "REFERENCES",
        "X" => "EXECUTE",
        "G" => "USAGE",
        "C" => "CREATE",
        "L" => "ALTER",
        "O" => "DROP",
        other => other,
    }
}

/// A DDL privilege on `SQL$TABLES`, `SQL$GENERATORS`… as Firebird writes
/// it: CREATE TABLE, ALTER ANY SEQUENCE, DROP DATABASE…
fn ddl_privilege(code: &str, target: &str) -> String {
    let noun = match target.trim_start_matches("SQL$") {
        "GENERATORS" => "SEQUENCE".to_string(),
        "CHARSETS" => "CHARACTER SET".to_string(),
        "DATABASE" => "DATABASE".to_string(),
        n => n.strip_suffix('S').unwrap_or(n).to_string(),
    };
    match code {
        "C" => format!("CREATE {noun}"),
        _ if noun == "DATABASE" => format!("{} DATABASE", verb(code)),
        _ => format!("{} ANY {noun}", verb(code)),
    }
}

pub async fn grants(s: &FirebirdSession, principal: &str) -> Result<Vec<Grant>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::from([(principal.to_string(), None)]);
    while let Some((name, via)) = queue.pop_front() {
        if !seen.insert(name.clone()) || seen.len() > 64 {
            continue;
        }
        for r in s.rows(GRANTS, vec![name.clone()]).await? {
            let code = r.first().and_then(text).unwrap_or_default();
            let target = r.get(2).and_then(text).unwrap_or_default();
            let column = r.get(3).and_then(text).filter(|c| !c.is_empty());
            let object_type = r.get(4).and_then(int).unwrap_or(0);
            let grantable = r.get(1).and_then(int).unwrap_or(0) != 0;
            let (privilege, object, object_kind) = if target.starts_with("SQL$") {
                (ddl_privilege(&code, &target), None, None)
            } else {
                let kind = match object_type {
                    0 if r.get(5).and_then(int) == Some(1) => "view",
                    0 => "table",
                    5 => "procedure",
                    14 => "sequence",
                    15 => "function",
                    17 => "exception",
                    18 => "package",
                    _ => "object",
                };
                let privilege = match column {
                    Some(c) => format!("{} ({c})", verb(&code)),
                    None => verb(&code).to_string(),
                };
                (privilege, Some(target), Some(kind.to_string()))
            };
            out.push(Grant { privilege, object, object_kind, grantable, denied: false, via: via.clone() });
        }
        for r in s.rows(ROLES_OF, vec![name.clone()]).await? {
            if let Some(role) = r.first().and_then(text) {
                let v = via.clone().unwrap_or_else(|| role.clone());
                queue.push_back((role, Some(v)));
            }
        }
    }
    Ok(out)
}

// -- scripts -----------------------------------------------------------------

/// A new user's or role's name: simple identifiers in upper case (as
/// Firebird folds them unquoted), anything else exactly as written.
fn new_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Query("falta el nombre".into()));
    }
    let simple = name.starts_with(|c: char| c.is_ascii_alphabetic()) && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'));
    Ok(q(&if simple { name.to_ascii_uppercase() } else { name.to_string() }))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn on(o: &ObjectRef) -> String {
    let prefix = match o.kind.as_str() {
        "procedure" => "PROCEDURE ",
        "function" => "FUNCTION ",
        "package" => "PACKAGE ",
        "sequence" => "SEQUENCE ",
        "exception" => "EXCEPTION ",
        _ => "",
    };
    format!("{prefix}{}", q(&o.name))
}

/// Privilege names: letters, spaces and underscores, optionally with a
/// column list (`UPDATE (a, b)`).
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let bad = |x: &str| Error::Query(format!("«{x}» no es un permiso de Firebird"));
    let mut out = Vec::new();
    for x in p {
        let (name, cols) = match x.split_once('(') {
            Some((n, c)) => (n.trim(), Some(c.trim().strip_suffix(')').ok_or_else(|| bad(x))?)),
            None => (x.trim(), None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '_') {
            return Err(bad(x));
        }
        let name = name.to_uppercase();
        match cols {
            Some(c) => {
                let cols: Vec<String> = c.split(',').map(|c| c.trim().trim_matches('"')).filter(|c| !c.is_empty()).map(q).collect();
                if cols.is_empty() {
                    return Err(bad(x));
                }
                out.push(format!("{name} ({})", cols.join(", ")));
            }
            None => out.push(name),
        }
    }
    Ok(out.join(", "))
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} PASSWORD {};", new_name(name)?, lit(pw))
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", new_name(name)?),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} PASSWORD {};", q(name), lit(password)),
        SecurityAction::SetLogin { name, enabled } => format!("ALTER USER {} {};", q(name), if *enabled { "ACTIVE" } else { "INACTIVE" }),
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            let on = object.as_ref().map(|o| format!(" ON {}", on(o))).unwrap_or_default();
            format!("GRANT {}{on} TO {}{};", privileges(p)?, q(to), if *grantable { " WITH GRANT OPTION" } else { "" })
        }
        SecurityAction::Revoke { privileges: p, object, from } => {
            let on = object.as_ref().map(|o| format!(" ON {}", on(o))).unwrap_or_default();
            format!("REVOKE {}{on} FROM {};", privileges(p)?, q(from))
        }
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
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w".into()) }), "CREATE USER \"ANA\" PASSWORD 'p''w';");
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "Mis Lectores".into() }), "CREATE ROLE \"Mis Lectores\";");
        assert_eq!(s(SecurityAction::SetLogin { name: "ANA".into(), enabled: false }), "ALTER USER \"ANA\" INACTIVE;");
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["select".into(), "UPDATE (TOTAL)".into()],
                object: Some(ObjectRef { kind: "table".into(), schema: None, name: "FACTURAS".into() }),
                to: "ANA".into(),
                grantable: true,
            }),
            "GRANT SELECT, UPDATE (\"TOTAL\") ON \"FACTURAS\" TO \"ANA\" WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["EXECUTE".into()], object: Some(ObjectRef { kind: "procedure".into(), schema: None, name: "P".into() }), from: "R".into() }),
            "REVOKE EXECUTE ON PROCEDURE \"P\" FROM \"R\";"
        );
        assert_eq!(s(SecurityAction::Grant { privileges: vec!["CREATE TABLE".into()], object: None, to: "ANA".into(), grantable: false }), "GRANT CREATE TABLE TO \"ANA\";");
        assert_eq!(s(SecurityAction::AddMember { role: "R".into(), member: "ANA".into() }), "GRANT \"R\" TO \"ANA\";");
        assert_eq!(s(SecurityAction::RemoveMember { role: "R".into(), member: "ANA".into() }), "REVOKE \"R\" FROM \"ANA\";");
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT; DROP TABLE x".into()], object: None, to: "a".into(), grantable: false }).is_err());
    }

    #[test]
    fn names_ddl_privileges() {
        assert_eq!(ddl_privilege("C", "SQL$TABLES"), "CREATE TABLE");
        assert_eq!(ddl_privilege("L", "SQL$GENERATORS"), "ALTER ANY SEQUENCE");
        assert_eq!(ddl_privilege("O", "SQL$DATABASE"), "DROP DATABASE");
    }
}

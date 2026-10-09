//! Users, roles and permissions (docs/users-and-permissions.md) for SAP HANA.
//!
//! Read from the SYS catalog: USERS, ROLES, GRANTED_ROLES and
//! GRANTED_PRIVILEGES (which lists only the direct grants of each grantee;
//! the ones held through roles are followed recursively). Without CATALOG
//! READ a user sees only its own grants and the ones it gave.
//!
//! Repository roles (`package::role`, created by _SYS_REPO) aren't granted
//! with `GRANT`: they go through `_SYS_REPO.GRANT_ACTIVATED_ROLE`.

use crate::{quote as q, text, HanaSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use hdbconnect_async::HdbValue;
use std::collections::{HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            // On schemas and objects.
            "SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "ALTER", "DROP", "INDEX", "TRIGGER", "REFERENCES",
            "CREATE ANY", "DEBUG", "ALL PRIVILEGES",
            // System privileges (granted without an object).
            "CATALOG READ", "CREATE SCHEMA", "USER ADMIN", "ROLE ADMIN", "DATA ADMIN", "BACKUP ADMIN", "MONITOR ADMIN",
            "INIFILE ADMIN", "TRACE ADMIN", "AUDIT ADMIN", "SESSION ADMIN", "EXPORT", "IMPORT",
        ],
        // The objects of the explorer have no schema: they're in the
        // connection's schema, which names them.
        object_kinds: vec!["", "schema", "table", "view", "procedure", "function", "sequence"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

// -- reading -----------------------------------------------------------------

const USERS: &str = "SELECT USER_NAME, USER_DEACTIVATED, CREATOR, TO_VARCHAR(CREATE_TIME, 'YYYY-MM-DD HH24:MI'),
       TO_VARCHAR(VALID_UNTIL, 'YYYY-MM-DD HH24:MI'), TO_VARCHAR(LAST_SUCCESSFUL_CONNECT, 'YYYY-MM-DD HH24:MI'),
       IS_PASSWORD_ENABLED, IS_RESTRICTED, USERGROUP_NAME
  FROM SYS.USERS ORDER BY USER_NAME";
/// Before 2.0 SPS03 (no user groups) and SPS09 (no restricted users).
const USERS_OLD: &str = "SELECT USER_NAME, USER_DEACTIVATED, CREATOR, TO_VARCHAR(CREATE_TIME, 'YYYY-MM-DD HH24:MI'),
       TO_VARCHAR(VALID_UNTIL, 'YYYY-MM-DD HH24:MI'), TO_VARCHAR(LAST_SUCCESSFUL_CONNECT, 'YYYY-MM-DD HH24:MI'),
       CAST(NULL AS NVARCHAR(5)), CAST(NULL AS NVARCHAR(5)), CAST(NULL AS NVARCHAR(5))
  FROM SYS.USERS ORDER BY USER_NAME";
const ROLES: &str = "SELECT ROLE_NAME, ROLE_SCHEMA_NAME, CREATOR, TO_VARCHAR(CREATE_TIME, 'YYYY-MM-DD HH24:MI')
  FROM SYS.ROLES ORDER BY ROLE_NAME";
const MEMBERS: &str = "SELECT GRANTEE, ROLE_NAME FROM SYS.GRANTED_ROLES";
const GRANTS: &str = "SELECT PRIVILEGE, OBJECT_TYPE, SCHEMA_NAME, OBJECT_NAME, COLUMN_NAME, IS_GRANTABLE
  FROM SYS.GRANTED_PRIVILEGES WHERE GRANTEE = ?";
const ROLES_OF: &str = "SELECT ROLE_NAME FROM SYS.GRANTED_ROLES WHERE GRANTEE = ?";

fn at(r: &[HdbValue<'static>], i: usize) -> Option<String> {
    r.get(i).and_then(text).filter(|s| !s.is_empty())
}

fn yes(v: Option<String>) -> bool {
    v.is_some_and(|v| v.eq_ignore_ascii_case("TRUE"))
}

/// Built-in users: SYS, SYSTEM, the _SYS_* ones and whatever SYS created.
fn system_user(name: &str, creator: Option<&str>) -> bool {
    name == "SYS" || name == "SYSTEM" || name.starts_with("_SYS") || creator == Some("SYS")
}

pub async fn principals(s: &HanaSession) -> Result<Vec<Principal>> {
    let users = match s.rows(USERS, &[]).await {
        Ok(r) => r,
        Err(_) => s.rows(USERS_OLD, &[]).await?,
    };
    let roles = s.rows(ROLES, &[]).await?;
    let mut out = Vec::new();
    for r in &users {
        let name = at(r, 0).unwrap_or_default();
        let creator = at(r, 2);
        let mut details = Vec::new();
        for (label, i) in [("Creado", 3), ("Vence", 4), ("Último ingreso", 5)] {
            if let Some(v) = at(r, i) {
                details.push((label.to_string(), v));
            }
        }
        if let Some(c) = &creator {
            details.push(("Creado por".into(), c.clone()));
        }
        if let Some(v) = at(r, 6) {
            details.push(("Contraseña".into(), if yes(Some(v)) { "habilitada".into() } else { "deshabilitada".into() }));
        }
        if yes(at(r, 7)) {
            details.push(("Restringido".into(), "sí".into()));
        }
        if let Some(g) = at(r, 8) {
            details.push(("Grupo de usuarios".into(), g));
        }
        out.push(Principal {
            kind: PrincipalKind::User,
            can_login: Some(true),
            superuser: Some(name == "SYSTEM"),
            disabled: Some(yes(at(r, 1))),
            member_of: Vec::new(),
            details,
            system: system_user(&name, creator.as_deref()),
            name,
        });
    }
    for r in &roles {
        let name = at(r, 0).unwrap_or_default();
        let creator = at(r, 2);
        let mut details = Vec::new();
        if let Some(sc) = at(r, 1) {
            details.push(("Esquema".into(), sc));
        }
        if let Some(c) = &creator {
            details.push(("Creado por".into(), c.clone()));
        }
        if let Some(t) = at(r, 3) {
            details.push(("Creado".into(), t));
        }
        if name.contains("::") {
            details.push(("Tipo".into(), "rol de repositorio".into()));
        }
        out.push(Principal {
            kind: PrincipalKind::Role,
            // Repository roles can't be dropped with DROP ROLE.
            system: name == "PUBLIC" || matches!(creator.as_deref(), Some("SYS" | "_SYS_REPO")),
            details,
            name,
            ..Default::default()
        });
    }
    for m in s.rows(MEMBERS, &[]).await.unwrap_or_default() {
        let (Some(member), Some(role)) = (at(&m, 0), at(&m, 1)) else { continue };
        if let Some(p) = out.iter_mut().find(|p| p.name == member) {
            if !p.member_of.contains(&role) {
                p.member_of.push(role);
            }
        }
    }
    Ok(out)
}

/// One row of GRANTED_PRIVILEGES as a grant.
fn grant_of(privilege: String, ty: &str, schema: Option<String>, object: Option<String>, column: Option<String>, grantable: bool) -> Grant {
    let (object, object_kind) = match ty {
        "SYSTEMPRIVILEGE" | "" => (None, None),
        "SCHEMA" => (schema.or(object), Some("schema".to_string())),
        _ => {
            let kind = match ty {
                "TABLE" | "VIEW" | "PROCEDURE" | "FUNCTION" | "SEQUENCE" => ty.to_ascii_lowercase(),
                // REMOTESOURCE, APPLICATIONPRIVILEGE, ANALYTICALPRIVILEGE, USERGROUP…
                other => other.to_ascii_lowercase().replace(' ', "_"),
            };
            let name = match (schema, object) {
                (Some(s), Some(o)) => Some(format!("{s}.{o}")),
                (s, o) => o.or(s),
            };
            (name, Some(kind))
        }
    };
    let privilege = match column {
        Some(c) => format!("{privilege} ({c})"),
        None => privilege,
    };
    Grant { privilege, object, object_kind, grantable, denied: false, via: None }
}

pub async fn grants(s: &HanaSession, principal: &str) -> Result<Vec<Grant>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::from([(principal.to_string(), None)]);
    while let Some((name, via)) = queue.pop_front() {
        if !seen.insert(name.clone()) || seen.len() > 256 {
            continue;
        }
        for r in s.rows(GRANTS, &[&name]).await? {
            let ty = at(&r, 1).unwrap_or_default();
            let mut g = grant_of(at(&r, 0).unwrap_or_default(), &ty, at(&r, 2), at(&r, 3), at(&r, 4), yes(at(&r, 5)));
            g.via = via.clone();
            out.push(g);
        }
        for r in s.rows(ROLES_OF, &[&name]).await? {
            if let Some(role) = at(&r, 0) {
                let v = via.clone().unwrap_or_else(|| role.clone());
                queue.push_back((role, Some(v)));
            }
        }
    }
    Ok(out)
}

// -- scripts -----------------------------------------------------------------

/// A new user's or role's name: a plain identifier is uppercased, as HANA
/// does with unquoted names; anything else is kept as typed.
fn new_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Query("escribí el nombre".into()));
    }
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    Ok(q(&if plain { name.to_ascii_uppercase() } else { name.to_string() }))
}

/// A password as HANA takes it: a quoted identifier, so it can't have `"`.
fn password(p: &str) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("escribí la contraseña del usuario".into()));
    }
    if p.contains('"') {
        return Err(Error::Query("en SAP HANA la contraseña no puede tener comillas dobles (\")".into()));
    }
    Ok(format!("\"{p}\""))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A repository role (`package::role`): granted through _SYS_REPO.
fn repo_role(role: &str) -> bool {
    role.contains("::")
}

fn on(o: &ObjectRef) -> String {
    match o.kind.as_str() {
        "schema" | "database" => format!("SCHEMA {}", q(&o.name)),
        _ => match o.schema() {
            Some(sc) => format!("{}.{}", q(sc), q(&o.name)),
            None => q(&o.name),
        },
    }
}

/// Privilege names: letters, spaces and underscores.
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out: Vec<String> = Vec::new();
    for x in p {
        let name = x.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '_') {
            return Err(Error::Query(format!("«{x}» no es un permiso de SAP HANA")));
        }
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase();
        if !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(out.join(", "))
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => format!(
            "CREATE USER {} PASSWORD {};\n\
             -- SAP HANA le pide cambiar la contraseña en el primer ingreso; para evitarlo, agregá NO FORCE_FIRST_PASSWORD_CHANGE.",
            new_name(name)?,
            password(pw.as_deref().unwrap_or_default())?
        ),
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", new_name(name)?),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!(
            "DROP USER {n};\n-- Si el usuario tiene objetos en su esquema, hace falta CASCADE, que los borra junto con él:\n-- DROP USER {n} CASCADE;",
            n = q(name)
        ),
        SecurityAction::SetPassword { name, password: pw } => format!("ALTER USER {} PASSWORD {};", q(name), password(pw)?),
        SecurityAction::SetLogin { name, enabled } => {
            format!("ALTER USER {} {} USER NOW;", q(name), if *enabled { "ACTIVATE" } else { "DEACTIVATE" })
        }
        SecurityAction::Grant { privileges: p, object: None, to, grantable } => {
            format!("GRANT {} TO {}{};", privileges(p)?, q(to), if *grantable { " WITH ADMIN OPTION" } else { "" })
        }
        SecurityAction::Grant { privileges: p, object: Some(o), to, grantable } => {
            format!("GRANT {} ON {} TO {}{};", privileges(p)?, on(o), q(to), if *grantable { " WITH GRANT OPTION" } else { "" })
        }
        SecurityAction::Revoke { privileges: p, object: None, from } => format!("REVOKE {} FROM {};", privileges(p)?, q(from)),
        SecurityAction::Revoke { privileges: p, object: Some(o), from } => {
            format!("REVOKE {} ON {} FROM {};", privileges(p)?, on(o), q(from))
        }
        SecurityAction::AddMember { role, member } if repo_role(role) => {
            format!("CALL \"_SYS_REPO\".\"GRANT_ACTIVATED_ROLE\"({}, {});", lit(role), lit(member))
        }
        SecurityAction::RemoveMember { role, member } if repo_role(role) => {
            format!("CALL \"_SYS_REPO\".\"REVOKE_ACTIVATED_ROLE\"({}, {});", lit(role), lit(member))
        }
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", q(role), q(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", q(role), q(member)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w\\1".into()) })
            .starts_with("CREATE USER \"ANA\" PASSWORD \"p'w\\1\";\n-- "));
        assert!(s(SecurityAction::CreateUser { name: "Ana López".into(), password: Some("x".into()) }).starts_with("CREATE USER \"Ana López\" PASSWORD"));
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: Some("a\"b".into()) }).is_err());
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert!(script(&SecurityAction::CreateRole { name: "  ".into() }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "lectores".into() }), "CREATE ROLE \"LECTORES\";");
        assert_eq!(s(SecurityAction::Drop { name: "LECT\"X".into(), kind: PrincipalKind::Role }), "DROP ROLE \"LECT\"\"X\";");
        assert!(s(SecurityAction::Drop { name: "ANA".into(), kind: PrincipalKind::User }).starts_with("DROP USER \"ANA\";\n-- "));
        assert_eq!(s(SecurityAction::SetPassword { name: "ANA".into(), password: "n'1".into() }), "ALTER USER \"ANA\" PASSWORD \"n'1\";");
        assert_eq!(s(SecurityAction::SetLogin { name: "ANA".into(), enabled: false }), "ALTER USER \"ANA\" DEACTIVATE USER NOW;");
        assert_eq!(s(SecurityAction::SetLogin { name: "ANA".into(), enabled: true }), "ALTER USER \"ANA\" ACTIVATE USER NOW;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["catalog  read".into()], object: None, to: "ANA".into(), grantable: true }),
            "GRANT CATALOG READ TO \"ANA\" WITH ADMIN OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "UPDATE".into()], object: obj("table", Some("VENTAS"), "FAC\"T"), to: "ANA".into(), grantable: true }),
            "GRANT SELECT, UPDATE ON \"VENTAS\".\"FAC\"\"T\" TO \"ANA\" WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("table", None, "T"), to: "R".into(), grantable: false }),
            "GRANT SELECT ON \"T\" TO \"R\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["INSERT".into()], object: obj("schema", None, "VENTAS"), from: "R".into() }),
            "REVOKE INSERT ON SCHEMA \"VENTAS\" FROM \"R\";"
        );
        assert_eq!(s(SecurityAction::Revoke { privileges: vec!["USER ADMIN".into()], object: None, from: "R".into() }), "REVOKE USER ADMIN FROM \"R\";");
        assert_eq!(s(SecurityAction::AddMember { role: "LECT".into(), member: "ANA".into() }), "GRANT \"LECT\" TO \"ANA\";");
        assert_eq!(s(SecurityAction::RemoveMember { role: "LECT".into(), member: "ANA".into() }), "REVOKE \"LECT\" FROM \"ANA\";");
        assert_eq!(
            s(SecurityAction::AddMember { role: "sap.hana::O'Admin".into(), member: "ANA".into() }),
            "CALL \"_SYS_REPO\".\"GRANT_ACTIVATED_ROLE\"('sap.hana::O''Admin', 'ANA');"
        );
        assert_eq!(
            s(SecurityAction::RemoveMember { role: "sap.hana::Admin".into(), member: "ANA".into() }),
            "CALL \"_SYS_REPO\".\"REVOKE_ACTIVATED_ROLE\"('sap.hana::Admin', 'ANA');"
        );
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT; DROP TABLE x".into()], object: None, to: "a".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec![], object: None, to: "a".into(), grantable: false }).is_err());
    }

    #[test]
    fn reads_granted_privileges() {
        let g = grant_of("CATALOG READ".into(), "SYSTEMPRIVILEGE", None, None, None, true);
        assert_eq!((g.object, g.object_kind, g.grantable), (None, None, true));
        let g = grant_of("SELECT".into(), "SCHEMA", Some("VENTAS".into()), None, None, false);
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref()), (Some("VENTAS"), Some("schema")));
        let g = grant_of("UPDATE".into(), "TABLE", Some("VENTAS".into()), Some("FACT".into()), Some("TOTAL".into()), false);
        assert_eq!((g.privilege.as_str(), g.object.as_deref(), g.object_kind.as_deref()), ("UPDATE (TOTAL)", Some("VENTAS.FACT"), Some("table")));
        let g = grant_of("CREATE VIRTUAL TABLE".into(), "REMOTESOURCE", None, Some("HANA2".into()), None, false);
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref()), (Some("HANA2"), Some("remotesource")));
    }
}

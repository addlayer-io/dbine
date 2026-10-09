//! Users and permissions of InfluxDB 1.x in InfluxQL
//! (docs/users-and-permissions.md): users are server-wide, either admins
//! (every privilege) or with READ / WRITE / ALL on each database. There are
//! no roles and users can't be disabled.
//!
//! InfluxDB 2 and 3 authorize with API tokens, not users with privileges a
//! query can change, so their drivers offer nothing here.

use crate::http::escape;
use crate::v1::{ident, InfluxQlSession};
use dbine_driver::{Error, Grant, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::Value as J;

/// Why InfluxDB 2 / 3 don't offer users and permissions.
pub const TOKENS: &str =
    "InfluxDB 2 y 3 autorizan con tokens de API, no con usuarios y permisos que se cambien con consultas: administralos desde la interfaz o la CLI de InfluxDB";

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["READ", "WRITE", "ALL"],
        object_kinds: vec!["", "database"],
        create_user: true,
        create_role: false,
        passwords: true,
        membership: false,
        per_database: false,
    }
}

fn rows(results: &[J]) -> Vec<Vec<J>> {
    let mut out = Vec::new();
    for r in results {
        for s in r.get("series").and_then(J::as_array).into_iter().flatten() {
            out.extend(s.get("values").and_then(J::as_array).into_iter().flatten().filter_map(|v| v.as_array().cloned()));
        }
    }
    out
}

fn text(row: &[J], i: usize) -> String {
    row.get(i).and_then(J::as_str).unwrap_or_default().to_string()
}

async fn admin_flag(s: &mut InfluxQlSession, user: &str) -> Result<Option<bool>> {
    let users = s.query("SHOW USERS").await?;
    Ok(rows(&users).iter().find(|r| text(r, 0) == user).map(|r| r.get(1).and_then(J::as_bool).unwrap_or(false)))
}

pub async fn principals(s: &mut InfluxQlSession) -> Result<Vec<Principal>> {
    let users = s.query("SHOW USERS").await?;
    Ok(rows(&users)
        .iter()
        .map(|r| {
            let admin = r.get(1).and_then(J::as_bool).unwrap_or(false);
            Principal {
                name: text(r, 0),
                kind: PrincipalKind::User,
                can_login: Some(true),
                superuser: Some(admin),
                disabled: None,
                member_of: Vec::new(),
                details: vec![("Tipo".into(), if admin { "Administrador" } else { "Usuario" }.into())],
                system: false,
            }
        })
        .collect())
}

pub async fn grants(s: &mut InfluxQlSession, principal: &str) -> Result<Vec<Grant>> {
    let mut out = Vec::new();
    if admin_flag(s, principal).await? == Some(true) {
        out.push(Grant { privilege: "ALL PRIVILEGES".into(), ..Default::default() });
    }
    let g = s.query(&format!("SHOW GRANTS FOR {}", ident(principal))).await?;
    for r in rows(&g) {
        let privilege = text(&r, 1);
        if privilege.is_empty() || privilege == "NO PRIVILEGES" {
            continue;
        }
        out.push(Grant {
            privilege,
            object: Some(text(&r, 0)),
            object_kind: Some("database".into()),
            ..Default::default()
        });
    }
    Ok(out)
}

// -- scripts -------------------------------------------------------------------

fn lit(s: &str) -> String {
    format!("'{}'", escape(s, '\''))
}

fn name(n: &str) -> Result<String> {
    if n.is_empty() || n.chars().any(char::is_control) {
        return Err(Error::Query("escribí un nombre válido".into()));
    }
    Ok(ident(n))
}

/// One InfluxQL privilege for a database: two of READ / WRITE make ALL
/// (a database holds a single privilege per user).
fn privilege(p: &[String]) -> Result<&'static str> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let (mut read, mut write) = (false, false);
    for x in p {
        match x.trim().to_ascii_uppercase().as_str() {
            "READ" => read = true,
            "WRITE" => write = true,
            "ALL" | "ALL PRIVILEGES" => (read, write) = (true, true),
            _ => return Err(Error::Query(format!("«{x}» no es un permiso de InfluxDB: READ, WRITE o ALL"))),
        }
    }
    Ok(match (read, write) {
        (true, true) => "ALL",
        (true, false) => "READ",
        _ => "WRITE",
    })
}

fn is_all(p: &[String]) -> bool {
    !p.is_empty() && p.iter().all(|x| matches!(x.trim().to_ascii_uppercase().as_str(), "ALL" | "ALL PRIVILEGES"))
}

const NO_ROLES: &str = "InfluxDB 1.x no tiene roles: los permisos se otorgan a cada usuario";

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name: n, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} WITH PASSWORD {}", name(n)?, lit(pw))
        }
        SecurityAction::Drop { name: n, kind: PrincipalKind::User } => format!("DROP USER {}", name(n)?),
        SecurityAction::SetPassword { name: n, password } => format!("SET PASSWORD FOR {} = {}", name(n)?, lit(password)),
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported(
                "InfluxDB 1.x no permite deshabilitar un usuario: cambiale la contraseña o borralo".into(),
            ))
        }
        SecurityAction::Grant { privileges, object, to, grantable } => {
            if *grantable {
                return Err(Error::Unsupported("en InfluxDB solo los administradores otorgan permisos; no se puede delegar".into()));
            }
            match object {
                None if is_all(privileges) => format!("GRANT ALL PRIVILEGES TO {}", name(to)?),
                None => return Err(Error::Query("elegí la base: en el servidor entero solo se otorga ALL (administrador)".into())),
                Some(db) => format!("GRANT {} ON {} TO {}", privilege(privileges)?, name(&db.name)?, name(to)?),
            }
        }
        SecurityAction::Revoke { privileges, object, from } => match object {
            None if is_all(privileges) => format!("REVOKE ALL PRIVILEGES FROM {}", name(from)?),
            None => return Err(Error::Query("elegí la base de la que se quita el permiso".into())),
            Some(db) => format!("REVOKE {} ON {} FROM {}", privilege(privileges)?, name(&db.name)?, name(from)?),
        },
        SecurityAction::CreateRole { .. }
        | SecurityAction::Drop { kind: PrincipalKind::Role, .. }
        | SecurityAction::AddMember { .. }
        | SecurityAction::RemoveMember { .. } => return Err(Error::Unsupported(NO_ROLES.into())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn db(n: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: "database".into(), schema: None, name: n.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "an\"a".into(), password: Some("p'w\\".into()) }),
            r#"CREATE USER "an\"a" WITH PASSWORD 'p\'w\\'"#
        );
        assert_eq!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), r#"DROP USER "ana""#);
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }), r#"SET PASSWORD FOR "ana" = 'x'"#);
        let grant = |p: &[&str], o| SecurityAction::Grant { privileges: p.iter().map(|x| x.to_string()).collect(), object: o, to: "ana".into(), grantable: false };
        assert_eq!(s(grant(&["read"], db("telemetría"))), r#"GRANT READ ON "telemetría" TO "ana""#);
        assert_eq!(s(grant(&["READ", "WRITE"], db("t"))), r#"GRANT ALL ON "t" TO "ana""#);
        assert_eq!(s(grant(&["ALL"], None)), r#"GRANT ALL PRIVILEGES TO "ana""#);
        assert!(script(&grant(&["READ"], None)).is_err());
        assert!(script(&grant(&["READ; DROP"], db("t"))).is_err());
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["WRITE".into()], object: db("t"), from: "ana".into() }),
            r#"REVOKE WRITE ON "t" FROM "ana""#
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["ALL PRIVILEGES".into()], object: None, from: "ana".into() }),
            r#"REVOKE ALL PRIVILEGES FROM "ana""#
        );
        for a in [
            SecurityAction::CreateRole { name: "r".into() },
            SecurityAction::AddMember { role: "r".into(), member: "ana".into() },
            SecurityAction::SetLogin { name: "ana".into(), enabled: false },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert!(script(&SecurityAction::Drop { name: "a\nb".into(), kind: PrincipalKind::User }).is_err());
    }
}

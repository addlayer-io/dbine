//! Users and permissions of a Cosmos DB database (docs/usuarios-y-permisos.md).
//!
//! The NoSQL API's own users (`/dbs/{db}/users`) don't sign in with a
//! password: an application with the account key asks for their resource
//! tokens, which carry each user's permissions (`All` or `Read` on a
//! container). Azure's role-based access (Entra ID, `az cosmosdb sql role`)
//! lives in the control plane, which the account key doesn't reach, so it
//! isn't managed here.
//!
//! The scripts are the driver's own commands (see `ddl::parse_admin`):
//! `CREATE USER`, `DROP USER`, `GRANT ALL|READ ON "container" TO "user"`
//! and `REVOKE … FROM …`. A permission's id is its container's name.

use crate::{enc, CosmosSession};
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::Value;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["ALL", "READ"],
        object_kinds: vec![kinds::COLLECTION],
        create_user: true,
        create_role: false,
        passwords: false,
        membership: false,
        per_database: true,
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

pub async fn principals(c: &CosmosSession) -> Result<Vec<Principal>> {
    let users = c.list(&format!("{}/users", c.db_path()?), "users", &c.db_link()?, "Users").await?;
    Ok(users
        .iter()
        .map(|u| Principal {
            name: s(u, "id"),
            kind: PrincipalKind::User,
            can_login: Some(false),
            details: vec![("Acceso".into(), "con los tokens de recurso que pide la aplicación con la clave de la cuenta".into())],
            ..Default::default()
        })
        .collect())
}

/// The permissions of a user (without their tokens, which are secrets).
pub(crate) async fn permissions(c: &CosmosSession, user: &str) -> Result<Vec<Value>> {
    let link = format!("{}/users/{user}", c.db_link()?);
    let path = format!("{}/users/{}/permissions", c.db_path()?, enc(user));
    let mut out = c.list(&path, "permissions", &link, "Permissions").await?;
    for p in &mut out {
        if let Some(o) = p.as_object_mut() {
            o.remove("_token");
        }
    }
    Ok(out)
}

/// A permission's resource (`dbs/<db>/colls/<c>[/docs/<d>…]`, by name or
/// by `_rid`) as the object it grants: the container, or the path under it.
fn resource_object(resource: &str, containers: &[(String, String)]) -> (Option<String>, Option<String>) {
    let tail = resource.trim_matches('/').split_once("/colls/").map(|(_, t)| t).unwrap_or(resource);
    let (coll, rest) = tail.split_once('/').map_or((tail, None), |(c, r)| (c, Some(r)));
    let name = containers.iter().find(|(_, rid)| rid.trim_end_matches('=') == coll.trim_end_matches('=') && !rid.is_empty()).map_or(coll, |(n, _)| n.as_str());
    match rest {
        None => (Some(name.to_string()), Some(kinds::COLLECTION.to_string())),
        Some(r) => (Some(format!("{name}/{r}")), Some("resource".to_string())),
    }
}

fn grant_of(p: &Value, containers: &[(String, String)]) -> Grant {
    let (object, object_kind) = resource_object(&s(p, "resource"), containers);
    Grant { privilege: s(p, "permissionMode").to_uppercase(), object, object_kind, ..Default::default() }
}

pub async fn grants(c: &CosmosSession, principal: &str) -> Result<Vec<Grant>> {
    let perms = permissions(c, principal).await?;
    let containers: Vec<(String, String)> = c
        .list(&format!("{}/colls", c.db_path()?), "colls", &c.db_link()?, "DocumentCollections")
        .await?
        .iter()
        .map(|x| (s(x, "id"), s(x, "_rid")))
        .collect();
    Ok(perms.iter().map(|p| grant_of(p, &containers)).collect())
}

// -- scripts -------------------------------------------------------------------

fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn mode(p: &[String]) -> Result<&'static str> {
    match p {
        [one] if one.trim().eq_ignore_ascii_case("all") => Ok("ALL"),
        [one] if one.trim().eq_ignore_ascii_case("read") => Ok("READ"),
        [] => Err(Error::Query("elegí ALL o READ".into())),
        _ => Err(Error::Query("Cosmos DB otorga un solo permiso por contenedor: ALL o READ".into())),
    }
}

fn container(object: &Option<ObjectRef>) -> Result<String> {
    match object {
        Some(o) if o.kind == kinds::COLLECTION => Ok(q(&o.name)),
        _ => Err(Error::Query("en Cosmos DB los permisos son sobre un contenedor: elegí uno".into())),
    }
}

/// A user id: no `/`, `\`, `?` or `#`, and not ending in a space.
fn check_id(name: &str) -> Result<()> {
    if name.is_empty() || name.contains(['/', '\\', '?', '#']) || name.ends_with(' ') {
        return Err(Error::Query(format!("«{name}» no es un id válido en Cosmos DB (no puede tener / \\ ? # ni terminar en espacio)")));
    }
    Ok(())
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, .. } => {
            check_id(name)?;
            format!("CREATE USER {};", q(name))
        }
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::Grant { privileges, object, to, grantable } => {
            if *grantable {
                return Err(Error::Unsupported("en Cosmos DB un usuario no otorga permisos a otros".into()));
            }
            format!("GRANT {} ON {} TO {};", mode(privileges)?, container(object)?, q(to))
        }
        SecurityAction::Revoke { privileges, object, from } => format!("REVOKE {} ON {} FROM {};", mode(privileges)?, container(object)?, q(from)),
        SecurityAction::SetPassword { .. } | SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported(
                "los usuarios de Cosmos DB no tienen contraseña ni ingreso: acceden con tokens de recurso. Para cortarle el acceso, revocale los permisos o borralo".into(),
            ))
        }
        SecurityAction::CreateRole { .. }
        | SecurityAction::Drop { kind: PrincipalKind::Role, .. }
        | SecurityAction::AddMember { .. }
        | SecurityAction::RemoveMember { .. } => {
            return Err(Error::Unsupported(
                "los roles de Cosmos DB (RBAC con Entra ID) se administran en Azure, no con la clave de la cuenta".into(),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::{parse_admin, Admin};
    use serde_json::json;

    fn coll(name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: name.into() })
    }

    #[test]
    fn scripts_parse_back() {
        let sc = |a| script(&a).unwrap();
        let create = sc(SecurityAction::CreateUser { name: "ana \"b\"".into(), password: None });
        assert_eq!(create, r#"CREATE USER "ana ""b""";"#);
        assert_eq!(parse_admin(create.trim_end_matches(';')).unwrap(), Some(Admin::CreateUser { name: "ana \"b\"".into() }));
        assert!(script(&SecurityAction::CreateUser { name: "a/b".into(), password: None }).is_err());
        let g = sc(SecurityAction::Grant { privileges: vec!["read".into()], object: coll("items"), to: "ana".into(), grantable: false });
        assert_eq!(g, r#"GRANT READ ON "items" TO "ana";"#);
        assert_eq!(parse_admin(g.trim_end_matches(';')).unwrap(), Some(Admin::Grant { mode: "Read".into(), container: "items".into(), user: "ana".into() }));
        let r = sc(SecurityAction::Revoke { privileges: vec!["ALL".into()], object: coll("it\"ems"), from: "ana".into() });
        assert_eq!(parse_admin(r.trim_end_matches(';')).unwrap(), Some(Admin::Revoke { mode: "All".into(), container: "it\"ems".into(), user: "ana".into() }));
        assert_eq!(sc(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), r#"DROP USER "ana";"#);
        assert!(parse_admin("GRANT WRITE ON c TO u").is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["ALL".into(), "READ".into()], object: coll("c"), to: "a".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["ALL".into()], object: None, to: "a".into(), grantable: false }).is_err());
        for a in [
            SecurityAction::SetPassword { name: "a".into(), password: "p".into() },
            SecurityAction::CreateRole { name: "r".into() },
            SecurityAction::AddMember { role: "r".into(), member: "a".into() },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn reads_permissions() {
        let containers = vec![("items".to_string(), "abcAAA==".to_string())];
        let g = grant_of(&json!({"id": "items", "permissionMode": "Read", "resource": "dbs/db/colls/items"}), &containers);
        assert_eq!((g.privilege.as_str(), g.object.as_deref(), g.object_kind.as_deref()), ("READ", Some("items"), Some(kinds::COLLECTION)));
        let g = grant_of(&json!({"permissionMode": "All", "resource": "dbs/xyz==/colls/abcAAA==/"}), &containers);
        assert_eq!(g.object.as_deref(), Some("items"));
        let g = grant_of(&json!({"permissionMode": "All", "resource": "dbs/db/colls/items/docs/d1"}), &containers);
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref()), (Some("items/docs/d1"), Some("resource")));
    }
}

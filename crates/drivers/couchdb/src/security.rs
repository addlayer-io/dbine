//! Users, roles and permissions of CouchDB (docs/users-and-permissions.md):
//!
//! - server admins (`/_node/_local/_config/admins`): every permission;
//! - users: `org.couchdb.user:<name>` documents in `_users`, each with its
//!   roles (plain names: a role exists while someone uses it);
//! - each database's `_security`: `admins` and `members`, by name or role.
//!
//! Scripts are the editor's HTTP console lines. Changing a user's document
//! (password, roles, delete) needs its current `_rev`, and `_security` is
//! replaced whole, so a static script can only create users: the rest is
//! done from the console, reading the document first.

use crate::{seg, CouchSession};
use dbine_driver::{Error, Grant, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use reqwest::Method;
use serde_json::{json, Value};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["admins", "members"],
        object_kinds: vec!["", "database"],
        create_user: true,
        create_role: false,
        passwords: true,
        membership: false,
        per_database: false,
    }
}

const USER_PREFIX: &str = "org.couchdb.user:";
/// Databases whose `_security` is read, at most.
const MAX_DATABASES: usize = 500;

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

/// What a server has: its admins, its `_users` documents and each
/// database's `_security`.
struct Snapshot {
    admins: Vec<String>,
    users: Vec<Value>,
    security: Vec<(String, Value)>,
}

async fn snapshot(s: &CouchSession) -> Result<Snapshot> {
    // Only admins can read the config: others just don't see the admins.
    let admins = match s.call(Method::GET, "/_node/_local/_config/admins", None).await {
        Ok(v) => v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let all = s
        .call(Method::GET, "/_users/_all_docs?include_docs=true&startkey=%22org.couchdb.user%3A%22&endkey=%22org.couchdb.user%3B%22", None)
        .await?;
    let users = all
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("doc").cloned())
        .filter(|d| d.get("_id").and_then(Value::as_str).is_some_and(|id| id.starts_with(USER_PREFIX)))
        .collect();
    let dbs = s.call(Method::GET, "/_all_dbs", None).await?;
    let mut security = Vec::new();
    for db in strs(Some(&dbs)).into_iter().filter(|d| !d.starts_with('_')).take(MAX_DATABASES) {
        if let Ok(v) = s.call(Method::GET, &format!("/{}/_security", seg(&db)), None).await {
            security.push((db, v));
        }
    }
    Ok(Snapshot { admins, users, security })
}

fn user_name(doc: &Value) -> String {
    doc.get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| doc.get("_id").and_then(Value::as_str).unwrap_or_default().trim_start_matches(USER_PREFIX).to_string())
}

fn principals_of(snap: &Snapshot) -> Vec<Principal> {
    let mut out: Vec<Principal> = Vec::new();
    for a in &snap.admins {
        out.push(Principal {
            name: a.clone(),
            kind: PrincipalKind::User,
            can_login: Some(true),
            superuser: Some(true),
            member_of: vec!["_admin".into()],
            details: vec![("Tipo".into(), "Administrador del servidor".into())],
            ..Default::default()
        });
    }
    let mut roles: Vec<String> = vec!["_admin".into()];
    let add_role = |r: &str, roles: &mut Vec<String>| {
        if !roles.iter().any(|x| x == r) {
            roles.push(r.to_string());
        }
    };
    for doc in &snap.users {
        let name = user_name(doc);
        let member_of = strs(doc.get("roles"));
        for r in &member_of {
            add_role(r, &mut roles);
        }
        let mut details = vec![("Tipo".into(), "Usuario de _users".into())];
        if !member_of.is_empty() {
            details.push(("Roles".into(), member_of.join(", ")));
        }
        if let Some(sch) = doc.get("password_scheme").and_then(Value::as_str) {
            details.push(("Contraseña".into(), sch.into()));
        }
        match out.iter_mut().find(|p| p.name == name) {
            // A server admin with a `_users` document too.
            Some(p) => {
                p.member_of.extend(member_of);
                p.details.extend(details.into_iter().skip(1));
            }
            None => out.push(Principal {
                name,
                kind: PrincipalKind::User,
                can_login: Some(true),
                superuser: Some(false),
                member_of,
                details,
                ..Default::default()
            }),
        }
    }
    for (_, sec) in &snap.security {
        for section in ["admins", "members"] {
            for r in strs(sec.get(section).and_then(|s| s.get("roles"))) {
                add_role(&r, &mut roles);
            }
        }
    }
    for r in roles {
        out.push(Principal {
            superuser: Some(r == "_admin"),
            system: r.starts_with('_'),
            details: vec![("Tipo".into(), if r == "_admin" { "Rol de los administradores del servidor" } else { "Rol" }.into())],
            name: r,
            kind: PrincipalKind::Role,
            ..Default::default()
        });
    }
    out
}

fn grants_of(snap: &Snapshot, principal: &str) -> Vec<Grant> {
    let user = snap.users.iter().find(|d| user_name(d) == principal);
    let admin = snap.admins.iter().any(|a| a == principal);
    let is_user = user.is_some() || admin;
    let mut roles = user.map(|d| strs(d.get("roles"))).unwrap_or_default();
    if admin {
        roles.insert(0, "_admin".into());
    }
    let mut out = Vec::new();
    if admin || (!is_user && principal == "_admin") {
        out.push(Grant { privilege: "_admin".into(), via: None, ..Default::default() });
    }
    for (db, sec) in &snap.security {
        for section in ["admins", "members"] {
            let part = sec.get(section);
            let db_grant = |via: Option<String>| Grant {
                privilege: section.into(),
                object: Some(db.clone()),
                object_kind: Some("database".into()),
                via,
                ..Default::default()
            };
            if is_user && strs(part.and_then(|p| p.get("names"))).iter().any(|n| n == principal) {
                out.push(db_grant(None));
            }
            for r in strs(part.and_then(|p| p.get("roles"))) {
                if is_user && roles.contains(&r) {
                    out.push(db_grant(Some(r)));
                } else if !is_user && r == principal {
                    out.push(db_grant(None));
                }
            }
        }
    }
    out
}

pub async fn principals(s: &CouchSession) -> Result<Vec<Principal>> {
    Ok(principals_of(&snapshot(s).await?))
}

pub async fn grants(s: &CouchSession, principal: &str) -> Result<Vec<Grant>> {
    Ok(grants_of(&snapshot(s).await?, principal))
}

// -- scripts -------------------------------------------------------------------

const NEEDS_REV: &str = "CouchDB exige la revisión actual (_rev) del documento del usuario para cambiarlo o borrarlo: hacelo desde la consola, con GET /_users/org.couchdb.user:<usuario> y después PUT o DELETE ?rev=…";

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            if name.is_empty() || name.starts_with('_') || name.contains(':') || name.chars().any(char::is_control) {
                return Err(Error::Query(format!("«{name}» no es un nombre de usuario válido en CouchDB (no puede empezar con _ ni tener :)")));
            }
            let doc = json!({ "name": name, "password": pw, "roles": [], "type": "user" });
            // Without `_rev`, an existing user answers "conflict": nothing is overwritten.
            format!(
                "PUT /_users/{}\n{}",
                seg(&format!("{USER_PREFIX}{name}")),
                serde_json::to_string_pretty(&doc).unwrap_or_default()
            )
        }
        SecurityAction::SetPassword { .. }
        | SecurityAction::Drop { kind: PrincipalKind::User, .. }
        | SecurityAction::AddMember { .. }
        | SecurityAction::RemoveMember { .. } => return Err(Error::Unsupported(NEEDS_REV.into())),
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported("CouchDB no permite deshabilitar un usuario: cambiale la contraseña o borralo".into()))
        }
        SecurityAction::CreateRole { .. } | SecurityAction::Drop { kind: PrincipalKind::Role, .. } => {
            return Err(Error::Unsupported(
                "en CouchDB los roles no se crean ni se borran: existen mientras algún usuario o alguna base los nombra".into(),
            ))
        }
        SecurityAction::Grant { .. } | SecurityAction::Revoke { .. } => {
            return Err(Error::Unsupported(
                "CouchDB guarda los permisos de cada base en un único documento _security que se reemplaza entero: editalo desde la consola, con GET /<base>/_security y después PUT /<base>/_security".into(),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_script, Stmt};

    #[test]
    fn create_user_is_a_console_line() {
        let s = script(&SecurityAction::CreateUser { name: "ana b".into(), password: Some("p\"w".into()) }).unwrap();
        let stmts = parse_script(&s).unwrap();
        assert_eq!(
            stmts,
            vec![Stmt::Http {
                method: "PUT".into(),
                path: "/_users/org.couchdb.user%3Aana%20b".into(),
                body: Some(json!({ "name": "ana b", "password": "p\"w", "roles": [], "type": "user" })),
            }]
        );
        assert!(script(&SecurityAction::CreateUser { name: "_x".into(), password: Some("p".into()) }).is_err());
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        for a in [
            SecurityAction::SetPassword { name: "a".into(), password: "p".into() },
            SecurityAction::Drop { name: "a".into(), kind: PrincipalKind::User },
            SecurityAction::CreateRole { name: "r".into() },
            SecurityAction::AddMember { role: "r".into(), member: "a".into() },
            SecurityAction::Grant { privileges: vec!["members".into()], object: None, to: "a".into(), grantable: false },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn reads_users_roles_and_database_permissions() {
        let snap = Snapshot {
            admins: vec!["admin".into()],
            users: vec![json!({ "_id": "org.couchdb.user:ana", "name": "ana", "roles": ["ventas"], "type": "user" })],
            security: vec![
                ("facturas".into(), json!({ "admins": { "names": ["ana"], "roles": [] }, "members": { "names": [], "roles": ["ventas", "_admin"] } })),
                ("otra".into(), json!({})),
            ],
        };
        let p = principals_of(&snap);
        assert!(p.iter().any(|p| p.name == "admin" && p.superuser == Some(true)));
        let ana = p.iter().find(|p| p.name == "ana").unwrap();
        assert_eq!(ana.member_of, vec!["ventas".to_string()]);
        assert!(p.iter().any(|p| p.name == "ventas" && p.kind == PrincipalKind::Role && !p.system));
        assert!(p.iter().any(|p| p.name == "_admin" && p.system));
        let g = grants_of(&snap, "ana");
        assert_eq!(g.len(), 2);
        assert!(g.iter().any(|g| g.privilege == "admins" && g.via.is_none() && g.object.as_deref() == Some("facturas")));
        assert!(g.iter().any(|g| g.privilege == "members" && g.via.as_deref() == Some("ventas")));
        let r = grants_of(&snap, "ventas");
        assert_eq!((r.len(), r[0].via.clone()), (1, None));
        let a = grants_of(&snap, "admin");
        assert!(a.iter().any(|g| g.privilege == "_admin" && g.object.is_none()));
        assert!(a.iter().any(|g| g.privilege == "members" && g.via.as_deref() == Some("_admin")));
    }
}

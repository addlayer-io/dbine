//! Users, roles and permissions (docs/usuarios-y-permisos.md) for Solr's
//! security.json plugins: Basic authentication (`/admin/authentication`,
//! the users and their passwords) and rule-based authorization
//! (`/admin/authorization`: each user's roles and the permissions, each
//! with the roles that hold it). Scripts are console requests
//! ([`crate::QUERY_HELP`]) posting the plugins' JSON commands.
//!
//! Roles aren't objects of their own (they exist while a user or a
//! permission names them) and both `set-user-role` and `set-permission`
//! replace a whole list: adding or removing one role or one permission
//! would overwrite what's there, so the script can't do it without reading
//! first. Those changes are left to the console.

use crate::SolrSession;
use dbine_driver::{kinds, Error, Grant, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use dbine_driver_elasticsearch::json::J;
use serde_json::json;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: Vec::new(),
        object_kinds: Vec::new(),
        create_user: true,
        create_role: false,
        passwords: true,
        membership: false,
        per_database: false,
    }
}

const AUTHENTICATION: &str = "/solr/admin/authentication";
const AUTHORIZATION: &str = "/solr/admin/authorization";

// -- reading ---------------------------------------------------------------

/// A string or a list of strings (`"role": "admin"` / `["a", "b"]`).
fn strs(v: Option<&J>) -> Vec<String> {
    match v {
        Some(J::Str(s)) => vec![s.clone()],
        Some(J::Arr(a)) => a.iter().filter_map(J::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// Basic-auth users (also under MultiAuthPlugin's `schemes`).
fn credentials(auth: &J) -> Vec<String> {
    let a = auth.get("authentication");
    let mut out: Vec<String> = Vec::new();
    let mut take = |c: Option<&J>| {
        if let Some(o) = c.and_then(J::as_obj) {
            out.extend(o.iter().map(|(k, _)| k.clone()));
        }
    };
    take(a.and_then(|a| a.get("credentials")));
    for s in a.and_then(|a| a.get("schemes")).and_then(J::as_arr).unwrap_or_default() {
        take(s.get("credentials"));
    }
    out
}

struct Authz {
    /// user -> roles
    user_roles: Vec<(String, Vec<String>)>,
    permissions: Vec<J>,
}

fn authz(j: &J) -> Authz {
    let a = j.get("authorization");
    Authz {
        user_roles: a
            .and_then(|a| a.get("user-role"))
            .and_then(J::as_obj)
            .map(|o| o.iter().map(|(u, r)| (u.clone(), strs(Some(r)))).collect())
            .unwrap_or_default(),
        permissions: a.and_then(|a| a.get("permissions")).and_then(J::as_arr).map(<[J]>::to_vec).unwrap_or_default(),
    }
}

/// `null` role: anyone, even without signing in; `*`: any signed-in user.
const ANYONE: &str = "cualquiera (sin autenticar)";
const SIGNED_IN: &str = "* (cualquier usuario autenticado)";

fn permission_roles(p: &J) -> Vec<String> {
    match p.get("role") {
        None | Some(J::Null) => vec![ANYONE.into()],
        r => strs(r).into_iter().map(|r| if r == "*" { SIGNED_IN.into() } else { r }).collect(),
    }
}

fn permission_grants(p: &J, via: Option<&str>) -> Vec<Grant> {
    let name = p.get("name").and_then(J::as_str).map(str::to_string);
    let privilege = name.unwrap_or_else(|| {
        let path = strs(p.get("path")).join(", ");
        let method = strs(p.get("method")).join(", ");
        if method.is_empty() { path } else { format!("{path} ({method})") }
    });
    let collections: Vec<Option<String>> = match p.get("collection") {
        Some(J::Null) | None => vec![None],
        c => {
            let c: Vec<Option<String>> = strs(c).into_iter().map(|c| (c != "*" && !c.is_empty()).then_some(c)).collect();
            if c.is_empty() { vec![None] } else { c }
        }
    };
    collections
        .into_iter()
        .map(|c| Grant {
            privilege: privilege.clone(),
            object_kind: c.is_some().then(|| kinds::COLLECTION.to_string()),
            object: c,
            grantable: false,
            denied: false,
            via: via.map(str::to_string),
        })
        .collect()
}

fn principals_of(auth: &J, z: &Authz) -> Vec<Principal> {
    let basic = credentials(auth);
    let admins: Vec<String> = z
        .permissions
        .iter()
        .filter(|p| p.get("name").and_then(J::as_str) == Some("all"))
        .flat_map(permission_roles)
        .collect();
    let mut users: Vec<String> = basic.clone();
    users.extend(z.user_roles.iter().map(|(u, _)| u.clone()));
    users.sort();
    users.dedup();
    let mut out: Vec<Principal> = users
        .into_iter()
        .map(|u| {
            let roles = z.user_roles.iter().find(|(n, _)| *n == u).map(|(_, r)| r.clone()).unwrap_or_default();
            let how = if basic.contains(&u) { "Básica (usuario y contraseña)" } else { "Externa (no tiene contraseña en Solr)" };
            Principal {
                superuser: Some(roles.iter().any(|r| admins.contains(r))),
                can_login: Some(basic.contains(&u)),
                disabled: None,
                member_of: roles,
                details: vec![("Tipo".into(), "Usuario".into()), ("Autenticación".into(), how.into())],
                system: false,
                kind: PrincipalKind::User,
                name: u,
            }
        })
        .collect();
    let mut roles: Vec<String> = z.user_roles.iter().flat_map(|(_, r)| r.clone()).collect();
    roles.extend(z.permissions.iter().flat_map(permission_roles).filter(|r| r != ANYONE && r != SIGNED_IN));
    roles.sort();
    roles.dedup();
    out.extend(roles.into_iter().map(|r| Principal {
        superuser: Some(admins.contains(&r)),
        details: vec![("Tipo".into(), "Rol (existe mientras un usuario o un permiso lo nombre)".into())],
        kind: PrincipalKind::Role,
        name: r,
        ..Default::default()
    }));
    out
}

fn grants_of(z: &Authz, principal: &str) -> Vec<Grant> {
    let roles = z.user_roles.iter().find(|(u, _)| u == principal).map(|(_, r)| r.clone());
    let mut out = Vec::new();
    for p in &z.permissions {
        let holders = permission_roles(p);
        match &roles {
            // A user: through its roles, `*` and `null`.
            Some(roles) => {
                for h in holders.iter().filter(|h| roles.contains(h) || *h == ANYONE || *h == SIGNED_IN) {
                    out.extend(permission_grants(p, Some(h)));
                }
            }
            None if holders.iter().any(|h| h == principal) => out.extend(permission_grants(p, None)),
            None => {}
        }
    }
    out
}

async fn read(s: &SolrSession) -> Result<(J, J)> {
    let auth = s.get_json(&format!("{AUTHENTICATION}?wt=json")).await?;
    let authz = s.get_json(&format!("{AUTHORIZATION}?wt=json")).await?;
    let on = |j: &J, k: &str| j.get(k).is_some();
    if !on(&auth, "authentication") && !on(&authz, "authorization") {
        return Err(Error::Unsupported(
            "este Solr no tiene seguridad configurada (security.json con los plugins de autenticación y autorización)".into(),
        ));
    }
    Ok((auth, authz))
}

pub async fn principals(s: &SolrSession) -> Result<Vec<Principal>> {
    let (auth, z) = read(s).await?;
    Ok(principals_of(&auth, &authz(&z)))
}

pub async fn grants(s: &SolrSession, principal: &str) -> Result<Vec<Grant>> {
    let (_, z) = read(s).await?;
    Ok(grants_of(&authz(&z), principal))
}

// -- scripts ---------------------------------------------------------------

fn body(v: serde_json::Value) -> String {
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

fn user_name(name: &str) -> Result<&str> {
    if name.trim().is_empty() || name.trim() != name || name.contains(':') || name.chars().any(char::is_control) {
        return Err(Error::Query(format!("«{name}» no es un nombre de usuario válido para la autenticación básica")));
    }
    Ok(name)
}

fn password(p: Option<&str>) -> Result<&str> {
    p.filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))
}

fn console_only(what: &str) -> Error {
    Error::Unsupported(format!(
        "Solr {what}: hay que reescribir la lista entera. Hacelo desde la consola, partiendo de GET {AUTHORIZATION}"
    ))
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => format!(
            "# Si el usuario ya existe, esto le cambia la contraseña. Para darle roles:\n\
             # POST {AUTHORIZATION}  {{\"set-user-role\": {{\"<usuario>\": [\"<rol>\"]}}}}\n\
             POST {AUTHENTICATION}\n{}",
            body(json!({ "set-user": { user_name(name)?: password(pw.as_deref())? } }))
        ),
        SecurityAction::SetPassword { name, password: pw } => {
            format!("POST {AUTHENTICATION}\n{}", body(json!({ "set-user": { user_name(name)?: password(Some(pw))? } })))
        }
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!(
            "POST {AUTHENTICATION}\n{}\n\n# Y sus roles:\nPOST {AUTHORIZATION}\n{}",
            body(json!({ "delete-user": [user_name(name)?] })),
            body(json!({ "set-user-role": { name.as_str(): null } }))
        ),
        SecurityAction::Drop { kind: PrincipalKind::Role, .. } | SecurityAction::CreateRole { .. } => {
            return Err(Error::Unsupported(
                "en Solr los roles no se crean ni se borran: existen mientras un usuario (set-user-role) o un permiso (set-permission) los nombre".into(),
            ))
        }
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported("la autenticación básica de Solr no permite deshabilitar un usuario: cambiale la contraseña o borralo".into()))
        }
        SecurityAction::Grant { .. } | SecurityAction::Revoke { .. } => {
            return Err(console_only("guarda en cada permiso la lista de roles que lo tienen (set-permission la reemplaza)"))
        }
        SecurityAction::AddMember { .. } | SecurityAction::RemoveMember { .. } => {
            return Err(console_only("guarda la lista de roles de cada usuario (set-user-role la reemplaza)"))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver_elasticsearch::console::{parse, Command};

    fn requests(script: &str) -> Vec<(String, String, serde_json::Value)> {
        parse(script)
            .unwrap()
            .into_iter()
            .map(|c| match c {
                Command::Http(r) => (r.method, r.path, serde_json::from_str(r.body.as_deref().unwrap()).unwrap()),
                Command::Sql(s) => panic!("SQL {s}"),
            })
            .collect()
    }

    #[test]
    fn scripts_are_console_requests() {
        let s = |a| script(&a).unwrap();
        let r = requests(&s(SecurityAction::CreateUser { name: "ana \"b\"".into(), password: Some("p\"w\n1".into()) }));
        assert_eq!(r, [("POST".into(), AUTHENTICATION.into(), json!({ "set-user": { "ana \"b\"": "p\"w\n1" } }))]);
        let r = requests(&s(SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }));
        assert_eq!(r[0].2, json!({ "set-user": { "ana": "x" } }));
        let r = requests(&s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }));
        assert_eq!(r[0], ("POST".into(), AUTHENTICATION.into(), json!({ "delete-user": ["ana"] })));
        assert_eq!(r[1], ("POST".into(), AUTHORIZATION.into(), json!({ "set-user-role": { "ana": null } })));
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert!(script(&SecurityAction::CreateUser { name: "a:b".into(), password: Some("x".into()) }).is_err());
        for a in [
            SecurityAction::CreateRole { name: "r".into() },
            SecurityAction::SetLogin { name: "a".into(), enabled: false },
            SecurityAction::Grant { privileges: vec!["read".into()], object: None, to: "r".into(), grantable: false },
            SecurityAction::AddMember { role: "r".into(), member: "a".into() },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn reads_security_json() {
        let auth = J::parse(r#"{"authentication":{"class":"solr.MultiAuthPlugin","schemes":[{"scheme":"basic","credentials":{"solr":"h s","ana":"h s"}}]}}"#).unwrap();
        let z = authz(&J::parse(r#"{"authorization":{"user-role":{"solr":"admin","ana":["lector"],"jwt-bob":["lector"]},
            "permissions":[{"name":"all","role":"admin"},{"name":"read","collection":["libros","*"],"role":["lector","admin"]},
                           {"name":"health","role":null},{"path":"/select","collection":"x","method":"GET","role":"*"}]}}"#).unwrap());
        let p = principals_of(&auth, &z);
        let names: Vec<(&str, PrincipalKind)> = p.iter().map(|p| (p.name.as_str(), p.kind)).collect();
        assert_eq!(names, [("ana", PrincipalKind::User), ("jwt-bob", PrincipalKind::User), ("solr", PrincipalKind::User), ("admin", PrincipalKind::Role), ("lector", PrincipalKind::Role)]);
        assert_eq!(p[2].superuser, Some(true));
        assert_eq!(p[1].can_login, Some(false));
        assert_eq!(p[0].member_of, ["lector"]);

        let g = grants_of(&z, "ana");
        let got: Vec<(&str, Option<&str>, Option<&str>)> = g.iter().map(|g| (g.privilege.as_str(), g.object.as_deref(), g.via.as_deref())).collect();
        assert_eq!(
            got,
            [("read", Some("libros"), Some("lector")), ("read", None, Some("lector")), ("health", None, Some(ANYONE)), ("/select (GET)", Some("x"), Some(SIGNED_IN))]
        );
        let g = grants_of(&z, "admin");
        assert_eq!(g.iter().map(|g| g.privilege.as_str()).collect::<Vec<_>>(), ["all", "read", "read"]);
        assert!(g.iter().all(|g| g.via.is_none()));
    }
}

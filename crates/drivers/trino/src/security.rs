//! Roles and permissions (docs/usuarios-y-permisos.md) for Trino, Presto
//! and Starburst.
//!
//! What there is depends on the access control the server and each catalog
//! are configured with: the connector has to manage roles and grants (Hive
//! with `hive.security=sql-standard`, for example) or the system access
//! control has to support system roles. Elsewhere (the default allow-all,
//! file-based rules, most connectors) there's nothing to manage and the tab
//! says so.
//!
//! Trino has no users of its own (they come from the authenticator:
//! password file, LDAP, OAuth…), so it can't create, drop, enable or change
//! the password of one. The users listed are the ones some grant or role
//! names, plus the session's.
//!
//! Roles live in a catalog (`CREATE ROLE r IN hive`) or in the system (no
//! `IN`). DBine names a catalog role `r IN catalog`, which the scripts turn
//! back into the `IN` clause; a plain name is a user (or a system role, when
//! it's the role of a membership).

use crate::{lit, TrinoSession};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::{HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "ALL PRIVILEGES"],
        object_kinds: vec!["schema", "table", "view"],
        create_user: false,
        create_role: true,
        passwords: false,
        membership: true,
        per_database: true,
    }
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

const IN: &str = " IN ";

/// `r IN catalog` → (`r`, Some(`catalog`)); a plain name → (name, None).
fn split_role(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once(IN) {
        Some((r, c)) if !r.trim().is_empty() && !c.trim().is_empty() => (r.trim(), Some(c.trim())),
        _ => (name.trim(), None),
    }
}

fn role_name(role: &str, catalog: &str) -> String {
    format!("{role}{IN}{catalog}")
}

fn no_users() -> Error {
    Error::Unsupported(
        "Trino no tiene usuarios propios: vienen del autenticador que tenga configurado (archivo de contraseñas, LDAP, OAuth…)".into(),
    )
}

fn not_managed(catalog: &str) -> Error {
    Error::Unsupported(format!(
        "el catálogo «{catalog}» no maneja roles ni permisos: depende del conector y del control de acceso configurado \
         en el servidor (por ejemplo, Hive con hive.security=sql-standard)"
    ))
}

// -- reading -----------------------------------------------------------------

fn cell(r: &[String], i: usize) -> Option<String> {
    r.get(i).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub async fn principals(s: &mut TrinoSession) -> Result<Vec<Principal>> {
    let cat = s.catalog()?;
    let is = format!("{}.information_schema", q(&cat));
    let mut managed = false;
    let mut out: Vec<Principal> = Vec::new();
    let role = |name: String, catalog_role: bool, out: &mut Vec<Principal>| {
        if !out.iter().any(|p| p.name == name) {
            out.push(Principal {
                kind: PrincipalKind::Role,
                // The connector's own: admin (Hive's) and public.
                system: matches!(name.split(IN).next(), Some("admin" | "public")),
                superuser: Some(name.split(IN).next() == Some("admin")),
                details: vec![("Ámbito".into(), if catalog_role { format!("catálogo {cat}") } else { "sistema".into() })],
                name,
                ..Default::default()
            });
        }
    };
    // System roles (Presto reads SHOW ROLES as the session catalog's). Trino
    // answers an empty list when they aren't enabled, so only some count.
    if s.flavor != crate::Flavor::Presto {
        if let Ok(rows) = s.strings("SHOW ROLES").await {
            managed |= !rows.is_empty();
            for r in rows.iter().filter_map(|r| cell(r, 0)) {
                role(r, false, &mut out);
            }
        }
    }
    if let Ok(rows) = s.strings(&format!("SHOW ROLES FROM {}", q(&cat))).await {
        managed = true;
        for r in rows.iter().filter_map(|r| cell(r, 0)) {
            role(role_name(&r, &cat), true, &mut out);
        }
    }
    // Memberships in the catalog's roles: grantee, grantee type, role.
    let members = match s.strings(&format!("SELECT grantee, grantee_type, role_name FROM {is}.role_authorization_descriptors")).await {
        Ok(r) => r,
        Err(_) => s.strings(&format!("SELECT grantee, grantee_type, role_name FROM {is}.applicable_roles")).await.unwrap_or_default(),
    };
    let privileged = s.strings(&format!("SELECT DISTINCT grantee, grantee_type FROM {is}.table_privileges")).await.unwrap_or_default();
    managed |= !privileged.is_empty();
    if !managed {
        return Err(not_managed(&cat));
    }
    let me = s.strings("SELECT current_user").await?.first().and_then(|r| cell(r, 0)).unwrap_or_default();
    let named = |r: &[String], i: usize| -> Option<String> {
        let name = cell(r, i)?;
        Some(if cell(r, i + 1).as_deref() == Some("ROLE") { role_name(&name, &cat) } else { name })
    };
    let mut users: Vec<String> = vec![me.clone()];
    for r in members.iter().chain(privileged.iter()) {
        if cell(r, 1).as_deref() == Some("USER") {
            users.extend(cell(r, 0));
        }
    }
    let mut seen = HashSet::new();
    for u in users.into_iter().filter(|u| !u.is_empty()) {
        if seen.insert(u.clone()) {
            let mut details = Vec::new();
            if u == me {
                details.push(("Nota".into(), "Usuario de esta conexión".into()));
            }
            out.push(Principal { name: u, kind: PrincipalKind::User, can_login: Some(true), details, ..Default::default() });
        }
    }
    for r in &members {
        let (Some(member), Some(role)) = (named(r, 0), cell(r, 2)) else { continue };
        let role = role_name(&role, &cat);
        if let Some(p) = out.iter_mut().find(|p| p.name == member) {
            if !p.member_of.contains(&role) {
                p.member_of.push(role);
            }
        }
    }
    Ok(out)
}

pub async fn grants(s: &mut TrinoSession, principal: &str) -> Result<Vec<Grant>> {
    let (name, catalog) = split_role(principal);
    let cat = match catalog {
        Some(c) => c.to_string(),
        None => s.catalog()?,
    };
    let is = format!("{}.information_schema", q(&cat));
    let kind = if catalog.is_some() { "ROLE" } else { "USER" };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, &str, Option<String>)> = VecDeque::from([(name.to_string(), kind, None)]);
    while let Some((grantee, ty, via)) = queue.pop_front() {
        if !seen.insert((grantee.clone(), ty)) || seen.len() > 256 {
            continue;
        }
        let who = format!("grantee = {} AND grantee_type = '{ty}'", lit(&grantee));
        let rows = match s
            .strings(&format!("SELECT privilege_type, table_schema, table_name, is_grantable FROM {is}.table_privileges WHERE {who}"))
            .await
        {
            Ok(r) => r,
            Err(_) if via.is_none() => return Err(not_managed(&cat)),
            Err(_) => continue,
        };
        for r in &rows {
            let object = match (cell(r, 1), cell(r, 2)) {
                (Some(sc), Some(t)) => Some(format!("{sc}.{t}")),
                (sc, t) => t.or(sc),
            };
            out.push(Grant {
                privilege: cell(r, 0).unwrap_or_default(),
                object,
                object_kind: Some("table".into()),
                grantable: cell(r, 3).is_some_and(|g| g.eq_ignore_ascii_case("YES")),
                denied: false,
                via: via.clone(),
            });
        }
        let roles = s.strings(&format!("SELECT role_name FROM {is}.role_authorization_descriptors WHERE {who}")).await.unwrap_or_default();
        for role in roles.iter().filter_map(|r| cell(r, 0)) {
            let v = via.clone().unwrap_or_else(|| role_name(&role, &cat));
            queue.push_back((role, "ROLE", Some(v)));
        }
    }
    Ok(out)
}

// -- scripts -----------------------------------------------------------------

/// A grantee: `ROLE "r"` for a catalog role, `USER "u"` otherwise.
fn grantee(name: &str) -> String {
    match split_role(name) {
        (r, Some(_)) => format!("ROLE {}", q(r)),
        (u, None) => format!("USER {}", q(u)),
    }
}

fn in_catalog(catalog: Option<&str>) -> String {
    catalog.map(|c| format!(" IN {}", q(c))).unwrap_or_default()
}

fn on(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else {
        return Err(Error::Query("en Trino los permisos se otorgan sobre un esquema o una tabla".into()));
    };
    Ok(match o.kind.as_str() {
        "schema" | "database" => format!("SCHEMA {}", q(&o.name)),
        _ => match o.schema() {
            Some(sc) => format!("TABLE {}.{}", q(sc), q(&o.name)),
            None => format!("TABLE {}", q(&o.name)),
        },
    })
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
            return Err(Error::Query(format!("«{x}» no es un permiso de Trino")));
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
        SecurityAction::CreateUser { .. }
        | SecurityAction::SetPassword { .. }
        | SecurityAction::SetLogin { .. }
        | SecurityAction::Drop { kind: PrincipalKind::User, .. } => return Err(no_users()),
        SecurityAction::CreateRole { name } => {
            let (r, c) = split_role(name);
            if r.is_empty() {
                return Err(Error::Query("escribí el nombre".into()));
            }
            format!("CREATE ROLE {}{};", q(r), in_catalog(c))
        }
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => {
            let (r, c) = split_role(name);
            format!("DROP ROLE {}{};", q(r), in_catalog(c))
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => format!(
            "GRANT {} ON {} TO {}{};",
            privileges(p)?,
            on(object)?,
            grantee(to),
            if *grantable { " WITH GRANT OPTION" } else { "" }
        ),
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {} ON {} FROM {};", privileges(p)?, on(object)?, grantee(from)),
        SecurityAction::AddMember { role, member } => {
            let (r, c) = split_role(role);
            format!("GRANT {} TO {}{};", q(r), grantee(member), in_catalog(c))
        }
        SecurityAction::RemoveMember { role, member } => {
            let (r, c) = split_role(role);
            format!("REVOKE {} FROM {}{};", q(r), grantee(member), in_catalog(c))
        }
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
        assert_eq!(s(SecurityAction::CreateRole { name: "analistas IN hive".into() }), "CREATE ROLE \"analistas\" IN \"hive\";");
        assert_eq!(s(SecurityAction::CreateRole { name: "global".into() }), "CREATE ROLE \"global\";");
        assert!(script(&SecurityAction::CreateRole { name: " ".into() }).is_err());
        assert_eq!(s(SecurityAction::Drop { name: "a\"b IN hive".into(), kind: PrincipalKind::Role }), "DROP ROLE \"a\"\"b\" IN \"hive\";");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "INSERT".into()], object: obj("table", Some("ventas"), "fact"), to: "ana".into(), grantable: true }),
            "GRANT SELECT, INSERT ON TABLE \"ventas\".\"fact\" TO USER \"ana\" WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("schema", None, "ventas"), to: "analistas IN hive".into(), grantable: false }),
            "GRANT SELECT ON SCHEMA \"ventas\" TO ROLE \"analistas\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["DELETE".into()], object: obj("view", Some("s"), "v"), from: "ana.b@x.com".into() }),
            "REVOKE DELETE ON TABLE \"s\".\"v\" FROM USER \"ana.b@x.com\";"
        );
        assert_eq!(
            s(SecurityAction::AddMember { role: "analistas IN hive".into(), member: "ana".into() }),
            "GRANT \"analistas\" TO USER \"ana\" IN \"hive\";"
        );
        assert_eq!(
            s(SecurityAction::RemoveMember { role: "analistas IN hive".into(), member: "jefes IN hive".into() }),
            "REVOKE \"analistas\" FROM ROLE \"jefes\" IN \"hive\";"
        );
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: None, to: "a".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT; DROP TABLE x".into()], object: obj("schema", None, "s"), to: "a".into(), grantable: false }).is_err());
        for a in [
            SecurityAction::CreateUser { name: "a".into(), password: Some("x".into()) },
            SecurityAction::SetPassword { name: "a".into(), password: "x".into() },
            SecurityAction::SetLogin { name: "a".into(), enabled: false },
            SecurityAction::Drop { name: "a".into(), kind: PrincipalKind::User },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn role_names_carry_their_catalog() {
        assert_eq!(split_role("analistas IN hive"), ("analistas", Some("hive")));
        assert_eq!(split_role("ana"), ("ana", None));
        assert_eq!(split_role(" IN hive"), (" IN hive".trim(), None));
        assert_eq!(role_name("r", "c"), "r IN c");
    }
}

//! Informix (and GBase 8s, its fork): the privileges of the current
//! database. `sysusers` has the users with a database-level privilege
//! (CONNECT, RESOURCE, DBA) and the roles (`usertype = 'G'`),
//! `sysroleauth` who holds each role, `systabauth` and `sysprocauth` the
//! privileges on tables, views and routines. Users are the operating
//! system's (or mapped by the server): SQL doesn't create them.

use super::{add_member, get, ident, privileges, rows, user, with_roles, Dialect, Row, EXTERNAL_USERS};
use crate::OdbcSession;
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashMap;

const D: Dialect = Dialect::Informix;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["CONNECT", "RESOURCE", "DBA", "SELECT", "INSERT", "UPDATE", "DELETE", "INDEX", "ALTER", "REFERENCES", "UNDER", "EXECUTE", "ALL"],
        // "" = the database.
        object_kinds: vec!["", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
        create_user: false,
        create_role: true,
        passwords: false,
        membership: true,
        per_database: true,
    }
}

fn level(t: &str) -> Option<&'static str> {
    Some(match t.trim().to_ascii_uppercase().as_str() {
        "D" => "DBA",
        "R" => "RESOURCE",
        "C" => "CONNECT",
        _ => return None,
    })
}

async fn memberships(s: &OdbcSession) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for r in rows(s, "SELECT rolename, grantee FROM sysroleauth").await.unwrap_or_default() {
        m.entry(get(&r, "grantee").to_ascii_lowercase()).or_default().push(get(&r, "rolename").to_string());
    }
    m
}

pub async fn principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let members = memberships(s).await;
    let mut out: Vec<Principal> = Vec::new();
    for u in rows(s, "SELECT username, usertype, defrole FROM sysusers").await? {
        let name = get(&u, "username").to_string();
        let ty = get(&u, "usertype");
        let member_of = members.get(&name.to_ascii_lowercase()).cloned().unwrap_or_default();
        if ty.eq_ignore_ascii_case("G") {
            out.push(Principal { member_of, system: name.eq_ignore_ascii_case("public"), ..super::role(D, &name) });
            continue;
        }
        let mut details = vec![("Autenticación".to_string(), "del sistema operativo (o usuario mapeado del servidor)".to_string())];
        if let Some(l) = level(ty) {
            details.push(("Nivel en la base".into(), l.into()));
        }
        if !get(&u, "defrole").is_empty() {
            details.push(("Rol predeterminado".into(), get(&u, "defrole").to_string()));
        }
        out.push(Principal {
            superuser: Some(ty.eq_ignore_ascii_case("D") || name.eq_ignore_ascii_case("informix")),
            can_login: Some(level(ty).is_some()),
            system: name.eq_ignore_ascii_case("informix") || name.eq_ignore_ascii_case("public"),
            member_of,
            details,
            ..user(&name)
        });
    }
    // Role holders without a row of their own in sysusers.
    for (grantee, roles) in &members {
        for r in roles {
            if !out.iter().any(|p| p.name.eq_ignore_ascii_case(grantee)) {
                add_member(&mut out, Principal { can_login: None, ..user(grantee) }, r.clone());
            }
        }
    }
    Ok(out)
}

/// `systabauth.tabauth`: one letter per privilege (upper case: grantable),
/// `-` where it's missing and `*` when there are column privileges.
pub(super) fn tabauth(v: &str) -> Vec<(&'static str, bool)> {
    v.chars()
        .filter_map(|c| {
            let p = match c.to_ascii_lowercase() {
                's' => "SELECT",
                'u' => "UPDATE",
                'i' => "INSERT",
                'd' => "DELETE",
                'x' => "INDEX",
                'a' => "ALTER",
                'r' => "REFERENCES",
                'n' => "UNDER",
                _ => return None,
            };
            Some((p, c.is_ascii_uppercase()))
        })
        .collect()
}

fn grants_of(users: &[Row], tabs: &[Row], procs: &[Row], name: &str, via: Option<String>) -> Vec<Grant> {
    let mut out = Vec::new();
    for u in users.iter().filter(|u| get(u, "username").eq_ignore_ascii_case(name)) {
        if let Some(l) = level(get(u, "usertype")) {
            out.push(Grant { privilege: l.into(), via: via.clone(), ..Default::default() });
        }
    }
    for t in tabs.iter().filter(|t| get(t, "grantee").eq_ignore_ascii_case(name)) {
        let object = format!("{}.{}", get(t, "owner"), get(t, "tabname"));
        let kind = if get(t, "tabtype").eq_ignore_ascii_case("V") { kinds::VIEW } else { kinds::TABLE };
        for (p, grantable) in tabauth(get(t, "tabauth")) {
            out.push(Grant { privilege: p.into(), object: Some(object.clone()), object_kind: Some(kind.into()), grantable, denied: false, via: via.clone() });
        }
    }
    for p in procs.iter().filter(|p| get(p, "grantee").eq_ignore_ascii_case(name)) {
        let auth = get(p, "procauth");
        let kind = if get(p, "isproc").eq_ignore_ascii_case("t") { kinds::PROCEDURE } else { kinds::FUNCTION };
        out.push(Grant {
            privilege: "EXECUTE".into(),
            object: Some(format!("{}.{}", get(p, "owner"), get(p, "procname"))),
            object_kind: Some(kind.into()),
            grantable: auth.starts_with('E'),
            denied: false,
            via: via.clone(),
        });
    }
    out
}

pub async fn grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let users = rows(s, "SELECT username, usertype FROM sysusers").await?;
    let tabs = rows(
        s,
        "SELECT a.grantee, a.tabauth, t.owner, t.tabname, t.tabtype FROM systabauth a JOIN systables t ON t.tabid = a.tabid",
    )
    .await
    .unwrap_or_default();
    let procs = rows(
        s,
        "SELECT a.grantee, a.procauth, p.owner, p.procname, p.isproc FROM sysprocauth a JOIN sysprocedures p ON p.procid = a.procid",
    )
    .await
    .unwrap_or_default();
    let members = memberships(s).await;
    Ok(with_roles(super::grantee(principal).1, &members, |n, via| grants_of(&users, &tabs, &procs, n, via)))
}

fn on(o: &ObjectRef) -> String {
    match o.kind.as_str() {
        k if k == kinds::PROCEDURE => format!(" ON PROCEDURE {}", super::qualified(D, o)),
        k if k == kinds::FUNCTION => format!(" ON FUNCTION {}", super::qualified(D, o)),
        _ => format!(" ON {}", super::qualified(D, o)),
    }
}

pub fn script(a: &SecurityAction) -> Result<String> {
    let id = |n: &str| ident(D, super::grantee(n).1);
    Ok(match a {
        SecurityAction::CreateUser { .. } | SecurityAction::SetPassword { .. } | SecurityAction::Drop { kind: PrincipalKind::User, .. } => {
            return Err(Error::Unsupported(EXTERNAL_USERS.into()))
        }
        // Without CONNECT a user can't open the database.
        SecurityAction::SetLogin { name, enabled: true } => format!("GRANT CONNECT TO {};", id(name)),
        SecurityAction::SetLogin { name, enabled: false } => format!("REVOKE CONNECT FROM {};", id(name)),
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", id(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", id(name)),
        SecurityAction::Grant { privileges: p, object: None, to, grantable } => {
            if *grantable {
                return Err(Error::Unsupported("Informix no otorga los permisos de la base (CONNECT, RESOURCE, DBA) con opción de otorgarlos a otros".into()));
            }
            format!("GRANT {} TO {};", privileges(p)?, id(to))
        }
        SecurityAction::Grant { privileges: p, object: Some(o), to, grantable } => {
            format!("GRANT {}{} TO {}{};", privileges(p)?, on(o), id(to), if *grantable { " WITH GRANT OPTION" } else { "" })
        }
        SecurityAction::Revoke { privileges: p, object: None, from } => format!("REVOKE {} FROM {};", privileges(p)?, id(from)),
        SecurityAction::Revoke { privileges: p, object: Some(o), from } => format!("REVOKE {}{} FROM {};", privileges(p)?, on(o), id(from)),
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", id(role), id(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", id(role), id(member)),
    })
}


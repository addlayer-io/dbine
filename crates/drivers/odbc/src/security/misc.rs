//! The rest of the presets with users in SQL:
//!
//! - **Virtuoso**: `DB.DBA.SYS_USERS` (users and roles), `SYS_ROLE_GRANTS`
//!   and `SYS_GRANTS` (a bit mask); users are made and changed with the
//!   `DB.DBA.USER_…` procedures.
//! - **Progress OpenEdge**: `PUB."_User"`, `SYSPROGRESS.SYSDBAUTH` (DBA,
//!   RESOURCE) and `SYSTABAUTH`. Its SQL has no roles to manage.
//! - **Machbase**: users only (`M$SYS_USERS`), with GRANT on tables; the
//!   catalog doesn't expose the grants.
//! - **Apache Ignite 2**: CREATE / ALTER / DROP USER, without GRANT nor a
//!   view that lists the users.
//! - **Ocient**: `sys.users`, `sys.groups`, the predefined `sys.roles`, their
//!   links and `sys.privileges`. GRANT names the grantee's kind (`TO USER`,
//!   `TO GROUP`): groups are `group:<name>` and roles `role:<name>`.

use super::{first, get, grantee, ident, lit, option, password, privileges, role, rows, unsupported_action, user, with_roles, Dialect, Row};
use crate::OdbcSession;
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashMap;

pub fn spec(d: Dialect) -> SecuritySpec {
    let base = SecuritySpec {
        privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "EXECUTE", "ALL"],
        object_kinds: vec![kinds::TABLE, kinds::VIEW, kinds::PROCEDURE],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    };
    match d {
        Dialect::Virtuoso => base,
        Dialect::OpenEdge => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "INDEX", "REFERENCES", "ALTER", "ALL", "DBA", "RESOURCE"],
            // "" = DBA and RESOURCE.
            object_kinds: vec!["", kinds::TABLE, kinds::VIEW],
            create_role: false,
            membership: false,
            ..base
        },
        Dialect::Machbase => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALL"],
            object_kinds: vec![kinds::TABLE],
            create_role: false,
            membership: false,
            ..base
        },
        Dialect::Ignite => SecuritySpec { privileges: vec![], object_kinds: vec![], create_role: false, membership: false, ..base },
        _ => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "DROP", "CREATE TABLE", "CREATE VIEW", "CREATE SCHEMA", "CREATE DATABASE", "ALL"],
            // "" = the system.
            object_kinds: vec!["", "schema", kinds::TABLE, kinds::VIEW],
            ..base
        },
    }
}

// Virtuoso ---------------------------------------------------------------------

const VIRT_MEMBERS: &str = "SELECT s.U_NAME AS grantee, r.U_NAME AS role_name FROM DB.DBA.SYS_ROLE_GRANTS g
  JOIN DB.DBA.SYS_USERS s ON s.U_ID = g.GI_SUPER JOIN DB.DBA.SYS_USERS r ON r.U_ID = g.GI_SUB WHERE g.GI_DIRECT = 1";

pub async fn virtuoso_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Virtuoso;
    let links = rows(s, VIRT_MEMBERS).await.unwrap_or_default();
    let of = |n: &str| -> Vec<String> { links.iter().filter(|l| get(l, "grantee") == n).map(|l| get(l, "role_name").to_string()).collect() };
    let mut out = Vec::new();
    for u in rows(s, "SELECT U_ID, U_NAME, U_IS_ROLE, U_ACCOUNT_DISABLED, U_SQL_ENABLE, U_GROUP, U_FULL_NAME FROM DB.DBA.SYS_USERS").await? {
        let name = get(&u, "u_name").to_string();
        if super::yes(get(&u, "u_is_role")) {
            out.push(Principal { member_of: of(&name), system: name == "dba" || name.starts_with("SPARQL_") || name.ends_with("_ADMIN"), ..role(d, &name) });
            continue;
        }
        let mut details = Vec::new();
        if !get(&u, "u_full_name").is_empty() {
            details.push(("Nombre".to_string(), get(&u, "u_full_name").to_string()));
        }
        out.push(Principal {
            superuser: Some(name == "dba" || get(&u, "u_group") == "0"),
            disabled: Some(super::yes(get(&u, "u_account_disabled"))),
            can_login: Some(super::yes(get(&u, "u_sql_enable"))),
            system: matches!(name.as_str(), "dba" | "dav" | "nobody" | "SPARQL"),
            member_of: of(&name),
            details,
            ..user(&name)
        });
    }
    Ok(out)
}

const VIRT_BITS: &[(i64, &str)] = &[(1, "SELECT"), (2, "UPDATE"), (4, "INSERT"), (8, "DELETE"), (32, "EXECUTE"), (64, "REFERENCES"), (256, "UNDER")];

pub(super) fn virtuoso_grants_of(r: &Row, via: &Option<String>) -> Vec<Grant> {
    let op: i64 = get(r, "g_op").parse().unwrap_or(0);
    let col = get(r, "g_col");
    let obj = get(r, "g_object");
    let (object, kind) = if col.is_empty() || col == "_" { (obj.to_string(), kinds::TABLE) } else { (format!("{obj}.{col}"), "column") };
    let kind = if op & 32 != 0 && op & 1 == 0 { kinds::PROCEDURE } else { kind };
    VIRT_BITS
        .iter()
        .filter(|(b, _)| op & b != 0)
        .map(|(_, p)| Grant { privilege: p.to_string(), object: Some(object.clone()), object_kind: Some(kind.into()), grantable: op & 16 != 0, denied: false, via: via.clone() })
        .collect()
}

pub async fn virtuoso_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let all = rows(s, "SELECT u.U_NAME AS grantee, g.G_OP, g.G_OBJECT, g.G_COL FROM DB.DBA.SYS_GRANTS g JOIN DB.DBA.SYS_USERS u ON u.U_ID = g.G_USER").await?;
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for l in rows(s, VIRT_MEMBERS).await.unwrap_or_default() {
        members.entry(get(&l, "grantee").to_ascii_lowercase()).or_default().push(get(&l, "role_name").to_string());
    }
    Ok(with_roles(grantee(principal).1, &members, |n, via| all.iter().filter(|r| get(r, "grantee") == n).flat_map(|r| virtuoso_grants_of(r, &via)).collect()))
}

// OpenEdge ---------------------------------------------------------------------

pub async fn openedge_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let dba = rows(s, "SELECT GRANTEE, DBA_ACC, RES_ACC FROM SYSPROGRESS.SYSDBAUTH").await.unwrap_or_default();
    let held = |n: &str, k: &str| dba.iter().any(|r| get(r, "grantee").eq_ignore_ascii_case(n) && !get(r, k).is_empty() && !get(r, k).eq_ignore_ascii_case("n"));
    let mut out = Vec::new();
    for n in first(s, "SELECT \"_Userid\" FROM PUB.\"_User\"").await? {
        out.push(Principal { superuser: Some(held(&n, "dba_acc")), ..user(&n) });
    }
    // DBA holders without a row in _User (the database's creator).
    for r in &dba {
        let n = get(r, "grantee");
        if !out.iter().any(|p| p.name.eq_ignore_ascii_case(n)) {
            out.push(Principal { superuser: Some(held(n, "dba_acc")), can_login: None, system: true, ..user(n) });
        }
    }
    Ok(out)
}

/// A SYSTABAUTH flag: empty or `n` isn't held, `g` is grantable.
pub(super) fn openedge_flag(v: &str) -> Option<bool> {
    let v = v.trim();
    (!v.is_empty() && !v.eq_ignore_ascii_case("n")).then(|| v.eq_ignore_ascii_case("g"))
}

pub async fn openedge_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let n = lit(grantee(principal).1);
    let mut out = Vec::new();
    for r in rows(s, &format!("SELECT DBA_ACC, RES_ACC FROM SYSPROGRESS.SYSDBAUTH WHERE GRANTEE = {n}")).await? {
        for (k, p) in [("dba_acc", "DBA"), ("res_acc", "RESOURCE")] {
            if openedge_flag(get(&r, k)).is_some() {
                out.push(Grant { privilege: p.into(), ..Default::default() });
            }
        }
    }
    for r in rows(s, &format!("SELECT * FROM SYSPROGRESS.SYSTABAUTH WHERE GRANTEE = {n}")).await.unwrap_or_default() {
        let object = Some(format!("{}.{}", get(&r, "tblowner"), get(&r, "tbl")));
        for (k, p) in [("sel", "SELECT"), ("ins", "INSERT"), ("upd", "UPDATE"), ("del", "DELETE"), ("ndx", "INDEX"), ("ref", "REFERENCES"), ("alt", "ALTER"), ("exe", "EXECUTE")] {
            if let Some(grantable) = openedge_flag(get(&r, k)) {
                out.push(Grant { privilege: p.into(), object: object.clone(), object_kind: Some(kinds::TABLE.into()), grantable, ..Default::default() });
            }
        }
    }
    Ok(out)
}

// Machbase, Ignite ---------------------------------------------------------------

pub async fn machbase_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    Ok(first(s, "SELECT NAME FROM M$SYS_USERS")
        .await?
        .into_iter()
        .map(|n| Principal { superuser: Some(n == "SYS"), system: n == "SYS", ..user(&n) })
        .collect())
}

pub fn ignite_principals() -> Vec<Principal> {
    vec![Principal {
        superuser: Some(true),
        system: true,
        details: vec![("Nota".into(), "Ignite no lista a los demás usuarios por SQL".into())],
        ..user("ignite")
    }]
}

// Ocient -----------------------------------------------------------------------

const GROUP: &str = "group:";

/// `(kind, name)`: 'u' user, 'g' group, 'r' role.
fn ocient_kind(n: &str) -> (char, &str) {
    if let Some(g) = n.strip_prefix(GROUP) {
        ('g', g)
    } else {
        match grantee(n) {
            (true, r) => ('r', r),
            (false, u) => ('u', u),
        }
    }
}

pub async fn ocient_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let ug = rows(s, "SELECT u.user_name, g.name FROM sys.user_groups x JOIN sys.users u ON u.id = x.user_id JOIN sys.groups g ON g.id = x.group_id").await.unwrap_or_default();
    let ur = rows(s, "SELECT u.user_name, r.name FROM sys.user_roles x JOIN sys.users u ON u.id = x.user_id JOIN sys.roles r ON r.id = x.role_id").await.unwrap_or_default();
    let gr = rows(s, "SELECT g.name AS group_name, r.name FROM sys.group_roles x JOIN sys.groups g ON g.id = x.group_id JOIN sys.roles r ON r.id = x.role_id").await.unwrap_or_default();
    let mut out = Vec::new();
    for u in rows(s, "SELECT user_name, state, invalid_login_attempts, password_days_remaining FROM sys.users").await? {
        let name = get(&u, "user_name").to_string();
        let mut member_of: Vec<String> = ug.iter().filter(|x| get(x, "user_name") == name).map(|x| format!("{GROUP}{}", get(x, "name"))).collect();
        member_of.extend(ur.iter().filter(|x| get(x, "user_name") == name).map(|x| super::role_name(Dialect::Ocient, get(x, "name"))));
        let state = get(&u, "state");
        out.push(Principal {
            superuser: Some(member_of.iter().any(|r| r.ends_with("System Administrator"))),
            disabled: Some(state.eq_ignore_ascii_case("DISABLED")),
            member_of,
            details: vec![("Estado".into(), state.to_string())],
            ..user(&name)
        });
    }
    for g in first(s, "SELECT name FROM sys.groups").await.unwrap_or_default() {
        let member_of = gr.iter().filter(|x| get(x, "group_name") == g).map(|x| super::role_name(Dialect::Ocient, get(x, "name"))).collect();
        out.push(Principal {
            name: format!("{GROUP}{g}"),
            kind: PrincipalKind::Role,
            can_login: Some(false),
            member_of,
            details: vec![("Tipo".into(), "grupo".into())],
            ..Default::default()
        });
    }
    for r in first(s, "SELECT name FROM sys.roles").await.unwrap_or_default() {
        out.push(Principal { system: true, details: vec![("Tipo".into(), "rol predefinido".into())], ..role(Dialect::Ocient, &r) });
    }
    Ok(out)
}

pub async fn ocient_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let all = rows(s, "SELECT grantee, privilege, privilege_target, object_type, grantable FROM sys.privileges").await?;
    let (_, name) = ocient_kind(principal);
    let groups = rows(
        s,
        &format!("SELECT g.name FROM sys.user_groups x JOIN sys.users u ON u.id = x.user_id JOIN sys.groups g ON g.id = x.group_id WHERE u.user_name = {}", lit(name)),
    )
    .await
    .unwrap_or_default();
    let of = |n: &str, via: Option<String>| -> Vec<Grant> {
        all.iter()
            .filter(|r| get(r, "grantee") == n)
            .map(|r| {
                let kind = get(r, "object_type").to_ascii_lowercase();
                let target = get(r, "privilege_target");
                Grant {
                    privilege: get(r, "privilege").to_uppercase(),
                    object: Some(target.to_string()).filter(|t| !t.is_empty() && kind != "system"),
                    object_kind: Some(kind).filter(|k| !k.is_empty() && k != "system"),
                    grantable: super::yes(get(r, "grantable")),
                    denied: false,
                    via: via.clone(),
                }
            })
            .collect()
    };
    let mut out = of(name, None);
    for g in groups {
        let g = get(&g, "name");
        out.extend(of(g, Some(format!("{GROUP}{g}"))));
    }
    Ok(out)
}

// scripts ----------------------------------------------------------------------

const NO_LOCK: &str = "este motor no bloquea ni deshabilita usuarios desde SQL: cambiale la contraseña o quitale los permisos";

fn on(d: Dialect, o: &ObjectRef) -> String {
    let q = super::qualified(d, o);
    match (d, o.kind.as_str()) {
        (Dialect::Ocient, "schema") => format!(" ON SCHEMA {}", ident(d, &o.name)),
        (Dialect::Ocient, k) if k == kinds::VIEW => format!(" ON VIEW {q}"),
        (Dialect::Ocient, _) => format!(" ON TABLE {q}"),
        _ => format!(" ON {q}"),
    }
}

fn ocient_whom(n: &str) -> Result<String> {
    let d = Dialect::Ocient;
    match ocient_kind(n) {
        ('g', g) => Ok(format!("GROUP {}", ident(d, g))),
        ('u', u) => Ok(format!("USER {}", ident(d, u))),
        _ => Err(Error::Query("en Ocient los permisos se otorgan a usuarios o grupos; los roles son predefinidos".into())),
    }
}

pub fn script(d: Dialect, a: &SecurityAction) -> Result<String> {
    let id = |n: &str| ident(d, grantee(n).1);
    let no_roles = || -> Result<String> {
        unsupported_action(match d {
            Dialect::OpenEdge => "el SQL de OpenEdge no crea roles ni les asigna miembros",
            Dialect::Machbase => "Machbase no tiene roles ni grupos",
            _ => "Ignite 2 no tiene roles, grupos ni permisos en SQL: solo usuarios",
        })
    };
    Ok(match (d, a) {
        // Virtuoso
        (Dialect::Virtuoso, SecurityAction::CreateUser { name, password: p }) => format!("DB.DBA.USER_CREATE({}, {});", lit(name), lit(password(p)?)),
        (Dialect::Virtuoso, SecurityAction::SetPassword { name, password: p }) => format!("DB.DBA.USER_SET_PASSWORD({}, {});", lit(name), lit(p)),
        (Dialect::Virtuoso, SecurityAction::SetLogin { name, enabled }) => format!("DB.DBA.USER_SET_OPTION({}, 'DISABLED', {});", lit(name), u8::from(!enabled)),
        (Dialect::Virtuoso, SecurityAction::Drop { name, kind: PrincipalKind::User }) => format!("DB.DBA.USER_DROP({});", lit(name)),
        // Ocient
        (Dialect::Ocient, SecurityAction::CreateUser { name, password: p }) => format!("CREATE USER {} PASSWORD = {};", id(name), lit(password(p)?)),
        (Dialect::Ocient, SecurityAction::SetPassword { name, password: p }) => format!("ALTER USER {} SET PASSWORD = {};", id(name), lit(p)),
        (Dialect::Ocient, SecurityAction::SetLogin { name, enabled }) => format!("ALTER USER {} {};", id(name), if *enabled { "ENABLE" } else { "DISABLE" }),
        (Dialect::Ocient, SecurityAction::CreateRole { name }) => format!("CREATE GROUP {};", ident(d, ocient_kind(name).1)),
        (Dialect::Ocient, SecurityAction::Drop { name, kind: PrincipalKind::Role }) => match ocient_kind(name) {
            ('r', _) => return unsupported_action("los roles de Ocient son predefinidos: no se borran"),
            (_, g) => format!("DROP GROUP {};", ident(d, g)),
        },
        (Dialect::Ocient, SecurityAction::AddMember { role, member }) => match ocient_kind(role) {
            ('r', r) => format!("GRANT ROLE {} TO {};", ident(d, r), ocient_whom(member)?),
            (_, g) => format!("ALTER GROUP {} ADD USER {};", ident(d, g), ident(d, ocient_kind(member).1)),
        },
        (Dialect::Ocient, SecurityAction::RemoveMember { role, member }) => match ocient_kind(role) {
            ('r', r) => format!("REVOKE ROLE {} FROM {};", ident(d, r), ocient_whom(member)?),
            (_, g) => format!("ALTER GROUP {} DROP USER {};", ident(d, g), ident(d, ocient_kind(member).1)),
        },
        (Dialect::Ocient, SecurityAction::Grant { privileges: p, object, to, grantable }) => {
            let on = object.as_ref().map_or_else(|| " ON SYSTEM".to_string(), |o| on(d, o));
            format!("GRANT {}{on} TO {}{};", privileges(p)?, ocient_whom(to)?, option(*grantable))
        }
        (Dialect::Ocient, SecurityAction::Revoke { privileges: p, object, from }) => {
            let on = object.as_ref().map_or_else(|| " ON SYSTEM".to_string(), |o| on(d, o));
            format!("REVOKE {}{on} FROM {};", privileges(p)?, ocient_whom(from)?)
        }
        // OpenEdge
        (Dialect::OpenEdge, SecurityAction::CreateUser { name, password: p }) => format!("CREATE USER {}, {};", lit(name), lit(password(p)?)),
        (Dialect::OpenEdge, SecurityAction::SetPassword { name, password: p }) => format!("ALTER USER {}, {};", lit(name), lit(p)),
        (Dialect::OpenEdge, SecurityAction::Drop { name, kind: PrincipalKind::User }) => format!("DROP USER {};", lit(name)),
        (Dialect::OpenEdge, SecurityAction::Grant { object: None, grantable: true, .. }) => {
            return unsupported_action("OpenEdge no otorga DBA ni RESOURCE con opción de otorgarlos a otros")
        }
        // Machbase
        (Dialect::Machbase, SecurityAction::CreateUser { name, password: p }) => format!("CREATE USER {} IDENTIFIED BY {};", id(name), lit(password(p)?)),
        (Dialect::Machbase, SecurityAction::SetPassword { name, password: p }) => format!("ALTER USER {} IDENTIFIED BY {};", id(name), lit(p)),
        (Dialect::Machbase, SecurityAction::Grant { grantable: true, .. }) => return unsupported_action("Machbase no otorga permisos con opción de otorgarlos a otros"),
        // Ignite
        (Dialect::Ignite, SecurityAction::CreateUser { name, password: p }) => format!("CREATE USER {} WITH PASSWORD {};", id(name), lit(password(p)?)),
        (Dialect::Ignite, SecurityAction::SetPassword { name, password: p }) => format!("ALTER USER {} WITH PASSWORD {};", id(name), lit(p)),
        (Dialect::Ignite, SecurityAction::Grant { .. } | SecurityAction::Revoke { .. }) => return no_roles(),
        // Shared
        (_, SecurityAction::SetLogin { .. }) => return unsupported_action(NO_LOCK),
        (_, SecurityAction::Drop { name, kind: PrincipalKind::User }) => format!("DROP USER {};", id(name)),
        (Dialect::Virtuoso, SecurityAction::CreateRole { name }) => format!("CREATE ROLE {};", id(name)),
        (Dialect::Virtuoso, SecurityAction::Drop { name, kind: PrincipalKind::Role }) => format!("DROP ROLE {};", id(name)),
        (Dialect::Virtuoso, SecurityAction::AddMember { role, member }) => format!("GRANT {} TO {};", id(role), id(member)),
        (Dialect::Virtuoso, SecurityAction::RemoveMember { role, member }) => format!("REVOKE {} FROM {};", id(role), id(member)),
        (_, SecurityAction::CreateRole { .. } | SecurityAction::Drop { .. } | SecurityAction::AddMember { .. } | SecurityAction::RemoveMember { .. }) => return no_roles(),
        (_, SecurityAction::Grant { privileges: p, object, to, grantable }) => {
            let on = match object {
                Some(o) => on(d, o),
                None if d == Dialect::OpenEdge => String::new(),
                None => return Err(Error::Query("elegí una tabla, una vista o un procedimiento".into())),
            };
            format!("GRANT {}{on} TO {}{};", privileges(p)?, id(to), option(*grantable))
        }
        (_, SecurityAction::Revoke { privileges: p, object, from }) => {
            let on = object.as_ref().map(|o| on(d, o)).unwrap_or_default();
            format!("REVOKE {}{on} FROM {};", privileges(p)?, id(from))
        }
        (_, SecurityAction::CreateUser { .. } | SecurityAction::SetPassword { .. }) => return unsupported_action("este motor no crea usuarios por SQL"),
    })
}

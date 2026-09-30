//! Engines whose roles are groups of users:
//!
//! - **CUBRID**: a group is a user with members (`db_user`, its
//!   `direct_groups`); privileges in `db_auth`, whose columns changed in
//!   11.2 and 11.4.
//! - **Actian Zen**: users and groups in `X$User` (bit 64 of `Xu$Flags`:
//!   a group), members from `psp_users`, rights on tables in `X$Rights` (a
//!   bit mask). Only with the database's security on.
//! - **Mimer SQL**: idents (`INFORMATION_SCHEMA.EXT_IDENTS`) of type USER,
//!   GROUP or PROGRAM; membership is the MEMBER privilege on a group.

use super::{get, grantee, ident, lit, option, password, privileges, role, rows, unsupported_action, user, with_roles, Dialect, Row};
use crate::OdbcSession;
use dbine_driver::{kinds, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::{HashMap, HashSet};

pub fn spec(d: Dialect) -> SecuritySpec {
    match d {
        Dialect::Cubrid => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "INDEX", "EXECUTE", "ALL PRIVILEGES"],
            object_kinds: vec![kinds::TABLE, kinds::VIEW, kinds::PROCEDURE],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
        Dialect::Zen => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "REFERENCES", "EXECUTE", "ALL", "CREATETAB", "CREATEVIEW", "CREATESP"],
            // "" = database rights (CREATETAB…).
            object_kinds: vec!["", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
        _ => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "EXECUTE", "USAGE", "ALL", "BACKUP", "DATABANK", "IDENT", "SCHEMA", "SHADOW", "STATISTICS"],
            // "" = system privileges.
            object_kinds: vec!["", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
    }
}

// CUBRID -----------------------------------------------------------------------

const CUBRID_MEMBERS: &str = "SELECT u.name AS member, g.name AS grp FROM db_user u, TABLE(u.direct_groups) AS t(g)";

pub async fn cubrid_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Cubrid;
    let links = rows(s, CUBRID_MEMBERS).await.unwrap_or_default();
    let groups: HashSet<String> = links.iter().map(|l| get(l, "grp").to_ascii_uppercase()).chain(["DBA".into(), "PUBLIC".into()]).collect();
    let mut out = Vec::new();
    for u in rows(s, "SELECT name FROM db_user").await? {
        let name = get(&u, "name").to_string();
        let member_of: Vec<String> = links.iter().filter(|l| get(l, "member").eq_ignore_ascii_case(&name)).map(|l| get(l, "grp").to_string()).collect();
        let base = if groups.contains(&name.to_ascii_uppercase()) { role(d, &name) } else { user(&name) };
        out.push(Principal {
            superuser: Some(name.eq_ignore_ascii_case("DBA") || member_of.iter().any(|g| g.eq_ignore_ascii_case("DBA"))),
            system: name.eq_ignore_ascii_case("DBA") || name.eq_ignore_ascii_case("PUBLIC"),
            member_of,
            ..base
        });
    }
    Ok(out)
}

/// A `db_auth` row: `object_name` (11.4) or `class_name`, with the owner
/// when the version has it.
pub(super) fn cubrid_grant(r: &Row, via: Option<String>) -> Grant {
    let name = [get(r, "object_name"), get(r, "class_name")].into_iter().find(|v| !v.is_empty()).unwrap_or_default();
    let owner = get(r, "owner_name");
    let object = if owner.is_empty() { name.to_string() } else { format!("{owner}.{name}") };
    let kind = match get(r, "object_type").to_ascii_uppercase().as_str() {
        "VIEW" | "VCLASS" => kinds::VIEW,
        "PROCEDURE" | "FUNCTION" => kinds::PROCEDURE,
        _ => kinds::TABLE,
    };
    Grant {
        privilege: get(r, "auth_type").to_string(),
        object: Some(object),
        object_kind: Some(kind.into()),
        grantable: super::yes(get(r, "is_grantable")),
        denied: false,
        via,
    }
}

pub async fn cubrid_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let auth = rows(s, "SELECT * FROM db_auth").await?;
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for l in rows(s, CUBRID_MEMBERS).await.unwrap_or_default() {
        members.entry(get(&l, "member").to_ascii_lowercase()).or_default().push(get(&l, "grp").to_string());
    }
    Ok(with_roles(grantee(principal).1, &members, |n, via| {
        auth.iter().filter(|r| get(r, "grantee_name").eq_ignore_ascii_case(n)).map(|r| cubrid_grant(r, via.clone())).collect()
    }))
}

// Zen --------------------------------------------------------------------------

/// `Xr$Rights` bits (0x80 marks the ones past SELECT).
pub(super) fn zen_rights(mask: u64) -> Vec<&'static str> {
    let mut out = Vec::new();
    if mask & 0x40 != 0 {
        out.push("SELECT");
    }
    for (bit, p) in [(0x02, "UPDATE"), (0x04, "INSERT"), (0x08, "DELETE"), (0x10, "REFERENCES"), (0x20, "ALTER")] {
        if mask & 0x80 != 0 && mask & bit != 0 {
            out.push(p);
        }
    }
    out
}

async fn zen_members(s: &OdbcSession) -> Vec<Row> {
    rows(s, "CALL psp_users(null, null, null)").await.unwrap_or_default()
}

pub async fn zen_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Zen;
    let members = zen_members(s).await;
    let mut out = Vec::new();
    for u in rows(s, "SELECT Xu$Name AS name, Xu$Flags AS flags FROM X$User").await? {
        let name = get(&u, "name").to_string();
        let flags: u64 = get(&u, "flags").parse().unwrap_or(0);
        if flags & 64 != 0 {
            out.push(Principal { system: name.eq_ignore_ascii_case("PUBLIC"), ..role(d, &name) });
            continue;
        }
        let member_of = members
            .iter()
            .filter(|m| get(m, "user_name").eq_ignore_ascii_case(&name))
            .map(|m| get(m, "group_name").to_string())
            .filter(|g| !g.is_empty())
            .collect();
        let mut details = Vec::new();
        if flags & 128 != 0 {
            details.push(("Puede crear tablas".to_string(), "sí".to_string()));
        }
        out.push(Principal {
            superuser: Some(name.eq_ignore_ascii_case("Master")),
            system: name.eq_ignore_ascii_case("Master"),
            member_of,
            details,
            ..user(&name)
        });
    }
    Ok(out)
}

pub async fn zen_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let rights = rows(
        s,
        "SELECT u.Xu$Name AS grantee, f.Xf$Name AS tabname, r.Xr$Rights AS rights
           FROM X$Rights r JOIN X$User u ON u.Xu$Id = r.Xr$User LEFT JOIN X$File f ON f.Xf$Id = r.Xr$Table
          WHERE r.Xr$Column IS NULL",
    )
    .await?;
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for m in zen_members(s).await {
        if !get(&m, "group_name").is_empty() {
            members.entry(get(&m, "user_name").to_ascii_lowercase()).or_default().push(get(&m, "group_name").to_string());
        }
    }
    Ok(with_roles(grantee(principal).1, &members, |n, via| {
        let mut out = Vec::new();
        for r in rights.iter().filter(|r| get(r, "grantee").eq_ignore_ascii_case(n)) {
            let mask: u64 = get(r, "rights").parse().unwrap_or(0);
            let table = get(r, "tabname");
            for p in zen_rights(mask) {
                out.push(Grant {
                    privilege: p.into(),
                    object: Some(table.to_string()).filter(|t| !t.is_empty()),
                    object_kind: Some(kinds::TABLE.into()),
                    via: via.clone(),
                    ..Default::default()
                });
            }
        }
        out
    }))
}

// Mimer ------------------------------------------------------------------------

const MIMER_MEMBERS: &str =
    "SELECT GRANTEE, OBJECT_NAME AS GRP, IS_GRANTABLE FROM INFORMATION_SCHEMA.EXT_OBJECT_PRIVILEGES WHERE PRIVILEGE_TYPE = 'MEMBER' AND OBJECT_TYPE = 'IDENT'";

pub async fn mimer_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Mimer;
    let links = rows(s, MIMER_MEMBERS).await.unwrap_or_default();
    let of = |n: &str| -> Vec<String> { links.iter().filter(|l| get(l, "grantee") == n).map(|l| get(l, "grp").to_string()).collect() };
    let mut out = Vec::new();
    for i in rows(s, "SELECT IDENT_NAME, IDENT_TYPE, HAS_PASSWORD, IDENT_LOGIN, IDENT_CREATOR FROM INFORMATION_SCHEMA.EXT_IDENTS").await? {
        let name = get(&i, "ident_name").to_string();
        let member_of = of(&name);
        let mut details = Vec::new();
        if !get(&i, "ident_creator").is_empty() {
            details.push(("Creado por".to_string(), get(&i, "ident_creator").to_string()));
        }
        match get(&i, "ident_type").to_ascii_uppercase().as_str() {
            "USER" => out.push(Principal {
                superuser: Some(name == "SYSADM"),
                can_login: Some(super::yes(get(&i, "has_password")) || !get(&i, "ident_login").is_empty()),
                system: name == "SYSADM" || name == "SYSTEM",
                member_of,
                details,
                ..user(&name)
            }),
            ty => {
                if ty == "PROGRAM" {
                    details.push(("Tipo".into(), "ident de programa (ENTER)".into()));
                }
                out.push(Principal { member_of, details, system: name == "PUBLIC", ..role(d, &name) });
            }
        }
    }
    Ok(out)
}

fn mimer_kind(t: &str) -> &'static str {
    match t.trim().to_ascii_uppercase().as_str() {
        "VIEW" => kinds::VIEW,
        "PROCEDURE" => kinds::PROCEDURE,
        "FUNCTION" => kinds::FUNCTION,
        "SCHEMA" => "schema",
        "SEQUENCE" => "sequence",
        "DOMAIN" => "domain",
        _ => kinds::TABLE,
    }
}

pub async fn mimer_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let sys = rows(s, "SELECT GRANTEE, PRIVILEGE_TYPE, IS_GRANTABLE FROM INFORMATION_SCHEMA.EXT_SYSTEM_PRIVILEGES").await?;
    let tabs = rows(
        s,
        "SELECT p.GRANTEE, p.TABLE_SCHEMA, p.TABLE_NAME, p.PRIVILEGE_TYPE, p.IS_GRANTABLE, t.TABLE_TYPE
           FROM INFORMATION_SCHEMA.TABLE_PRIVILEGES p
           LEFT JOIN INFORMATION_SCHEMA.TABLES t ON t.TABLE_SCHEMA = p.TABLE_SCHEMA AND t.TABLE_NAME = p.TABLE_NAME",
    )
    .await
    .unwrap_or_default();
    let objs = rows(
        s,
        "SELECT GRANTEE, OBJECT_SCHEMA, OBJECT_NAME, OBJECT_TYPE, PRIVILEGE_TYPE, IS_GRANTABLE
           FROM INFORMATION_SCHEMA.EXT_OBJECT_PRIVILEGES WHERE PRIVILEGE_TYPE <> 'MEMBER'",
    )
    .await
    .unwrap_or_default();
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for l in rows(s, MIMER_MEMBERS).await.unwrap_or_default() {
        members.entry(get(&l, "grantee").to_ascii_lowercase()).or_default().push(get(&l, "grp").to_string());
    }
    Ok(with_roles(grantee(principal).1, &members, |n, via| {
        let mut out: Vec<Grant> = sys
            .iter()
            .filter(|r| get(r, "grantee") == n)
            .map(|r| Grant { privilege: get(r, "privilege_type").to_string(), grantable: super::yes(get(r, "is_grantable")), via: via.clone(), ..Default::default() })
            .collect();
        for r in tabs.iter().filter(|r| get(r, "grantee") == n) {
            let kind = if get(r, "table_type").eq_ignore_ascii_case("VIEW") { kinds::VIEW } else { kinds::TABLE };
            out.push(Grant {
                privilege: get(r, "privilege_type").to_string(),
                object: Some(format!("{}.{}", get(r, "table_schema"), get(r, "table_name"))),
                object_kind: Some(kind.into()),
                grantable: super::yes(get(r, "is_grantable")),
                denied: false,
                via: via.clone(),
            });
        }
        for r in objs.iter().filter(|r| get(r, "grantee") == n) {
            let schema = get(r, "object_schema");
            let name = get(r, "object_name");
            out.push(Grant {
                privilege: get(r, "privilege_type").to_string(),
                object: Some(if schema.is_empty() { name.to_string() } else { format!("{schema}.{name}") }),
                object_kind: Some(mimer_kind(get(r, "object_type")).into()),
                grantable: super::yes(get(r, "is_grantable")),
                denied: false,
                via: via.clone(),
            });
        }
        out
    }))
}

// scripts ----------------------------------------------------------------------

fn on(d: Dialect, o: &ObjectRef) -> String {
    let q = super::qualified(d, o);
    match (d, o.kind.as_str()) {
        (Dialect::Zen, k) if k == kinds::VIEW => format!(" ON VIEW {q}"),
        (Dialect::Zen, k) if k == kinds::PROCEDURE => format!(" ON PROCEDURE {q}"),
        (Dialect::Mimer | Dialect::Cubrid, k) if k == kinds::PROCEDURE => format!(" ON PROCEDURE {q}"),
        (Dialect::Mimer, k) if k == kinds::FUNCTION => format!(" ON FUNCTION {q}"),
        _ => format!(" ON {q}"),
    }
}

/// Zen's passwords are delimited like identifiers.
fn quoted_pw(p: &str) -> String {
    format!("\"{}\"", p.replace('"', "\"\""))
}

const NO_LOCK: &str = "este motor no bloquea ni deshabilita usuarios desde SQL: cambiale la contraseña o quitale los permisos";

pub fn script(d: Dialect, a: &SecurityAction) -> Result<String> {
    let id = |n: &str| ident(d, grantee(n).1);
    Ok(match (d, a) {
        (_, SecurityAction::SetLogin { .. }) => return unsupported_action(NO_LOCK),
        // CUBRID
        (Dialect::Cubrid, SecurityAction::CreateUser { name, password: p }) => format!("CREATE USER {} PASSWORD {};", id(name), lit(password(p)?)),
        (Dialect::Cubrid, SecurityAction::SetPassword { name, password: p }) => format!("ALTER USER {} PASSWORD {};", id(name), lit(p)),
        (Dialect::Cubrid, SecurityAction::CreateRole { name }) => format!("CREATE USER {};", id(name)),
        (Dialect::Cubrid, SecurityAction::Drop { name, .. }) => format!("DROP USER {};", id(name)),
        (Dialect::Cubrid, SecurityAction::AddMember { role, member }) => format!("ALTER USER {} ADD MEMBERS {};", id(role), id(member)),
        (Dialect::Cubrid, SecurityAction::RemoveMember { role, member }) => format!("ALTER USER {} DROP MEMBERS {};", id(role), id(member)),
        // Zen
        (Dialect::Zen, SecurityAction::CreateUser { name, password: p }) => format!("CREATE USER {} WITH PASSWORD {};", id(name), quoted_pw(password(p)?)),
        (Dialect::Zen, SecurityAction::SetPassword { name, password: p }) => format!("ALTER USER {} WITH PASSWORD {};", id(name), quoted_pw(p)),
        (Dialect::Zen, SecurityAction::CreateRole { name }) => format!("CREATE GROUP {};", id(name)),
        (Dialect::Zen, SecurityAction::Drop { name, kind: PrincipalKind::User }) => format!("DROP USER {};", id(name)),
        (Dialect::Zen, SecurityAction::Drop { name, kind: PrincipalKind::Role }) => format!("DROP GROUP {};", id(name)),
        (Dialect::Zen, SecurityAction::AddMember { role, member }) => format!("ALTER GROUP {} ADD USER {};", id(role), id(member)),
        (Dialect::Zen, SecurityAction::RemoveMember { role, member }) => format!("ALTER GROUP {} DROP USER {};", id(role), id(member)),
        (Dialect::Zen, SecurityAction::Grant { grantable: true, .. }) => return unsupported_action("Zen no otorga permisos con opción de otorgarlos a otros"),
        // Mimer
        (_, SecurityAction::CreateUser { name, password: p }) => format!("CREATE IDENT {} AS USER USING {};", id(name), lit(password(p)?)),
        (_, SecurityAction::SetPassword { name, password: p }) => format!("ALTER IDENT {} SET PASSWORD {};", id(name), lit(p)),
        (_, SecurityAction::CreateRole { name }) => format!("CREATE IDENT {} AS GROUP;", id(name)),
        (_, SecurityAction::Drop { name, .. }) => format!("DROP IDENT {};", id(name)),
        (_, SecurityAction::AddMember { role, member }) => format!("GRANT MEMBER ON {} TO {};", id(role), id(member)),
        (_, SecurityAction::RemoveMember { role, member }) => format!("REVOKE MEMBER ON {} FROM {};", id(role), id(member)),
        // GRANT / REVOKE, the same in the three.
        (_, SecurityAction::Grant { privileges: p, object, to, grantable }) => {
            let on = match object {
                Some(o) => on(d, o),
                None if d == Dialect::Cubrid => return Err(dbine_driver::Error::Query("elegí una tabla, una vista o un procedimiento: CUBRID no tiene permisos del sistema".into())),
                None => String::new(),
            };
            format!("GRANT {}{on} TO {}{};", privileges(p)?, id(to), option(*grantable))
        }
        (_, SecurityAction::Revoke { privileges: p, object, from }) => {
            let on = match object {
                Some(o) => on(d, o),
                None if d == Dialect::Cubrid => return Err(dbine_driver::Error::Query("elegí una tabla, una vista o un procedimiento".into())),
                None => String::new(),
            };
            format!("REVOKE {}{on} FROM {};", privileges(p)?, id(from))
        }
    })
}

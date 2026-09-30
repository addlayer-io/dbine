//! Altibase and Dameng, whose users, roles and GRANTs follow Oracle's:
//! `CREATE USER … IDENTIFIED BY`, `ALTER USER … ACCOUNT LOCK`, `GRANT
//! <role> TO <user>`, system privileges without `ON`.
//!
//! - **Altibase**: `SYSTEM_.SYS_USERS_` (users and roles, `USER_TYPE` U/R,
//!   `ACCOUNT_LOCK` L), `SYS_USER_ROLES_`, `SYS_GRANT_SYSTEM_` and
//!   `SYS_GRANT_OBJECT_` with the names of `SYS_PRIVILEGES_`.
//! - **Dameng**: the Oracle-style views `DBA_USERS`, `DBA_ROLE_PRIVS`,
//!   `DBA_SYS_PRIVS` and `DBA_TAB_PRIVS`; roles from `SYSOBJECTS`.

use super::{first, get, grantee, ident, option, password, privileges, role, rows, unsupported_action, user, with_roles, Dialect, Row};
use crate::OdbcSession;
use dbine_driver::{kinds, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashMap;

pub fn spec(d: Dialect) -> SecuritySpec {
    let mut privileges = vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "INDEX", "REFERENCES", "EXECUTE", "ALL"];
    privileges.extend([
        "CREATE SESSION", "CREATE TABLE", "CREATE VIEW", "CREATE PROCEDURE", "CREATE SEQUENCE", "CREATE ANY TABLE", "SELECT ANY TABLE",
        "CREATE USER", "ALTER USER", "DROP USER",
    ]);
    if d == Dialect::Dameng {
        privileges.extend(["CREATE SCHEMA", "BACKUP DATABASE"]);
    }
    SecuritySpec {
        privileges,
        // "" = system privileges.
        object_kinds: vec!["", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

fn members_map(rs: &[Row]) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for r in rs {
        m.entry(get(r, "grantee").to_ascii_lowercase()).or_default().push(get(r, "granted_role").to_string());
    }
    m
}

// Altibase ---------------------------------------------------------------------

const ALTIBASE_MEMBERS: &str = "SELECT u.USER_NAME AS GRANTEE, r.USER_NAME AS GRANTED_ROLE
  FROM SYSTEM_.SYS_USER_ROLES_ x JOIN SYSTEM_.SYS_USERS_ u ON u.USER_ID = x.GRANTEE_ID JOIN SYSTEM_.SYS_USERS_ r ON r.USER_ID = x.ROLE_ID";

pub async fn altibase_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Altibase;
    let members = members_map(&rows(s, ALTIBASE_MEMBERS).await.unwrap_or_default());
    let mut out = Vec::new();
    for u in rows(s, "SELECT * FROM SYSTEM_.SYS_USERS_").await? {
        let name = get(&u, "user_name").to_string();
        let member_of = members.get(&name.to_ascii_lowercase()).cloned().unwrap_or_default();
        if get(&u, "user_type").eq_ignore_ascii_case("R") {
            out.push(Principal { member_of, ..role(d, &name) });
            continue;
        }
        let mut details = Vec::new();
        for (k, label) in [("created", "Alta"), ("password_expiry_date", "Vence la contraseña"), ("account_lock_date", "Bloqueado el")] {
            let v = get(&u, k);
            if !v.is_empty() {
                details.push((label.to_string(), v.to_string()));
            }
        }
        out.push(Principal {
            superuser: Some(name == "SYS" || name == "SYSTEM_"),
            disabled: Some(get(&u, "account_lock").eq_ignore_ascii_case("L")),
            system: name == "SYS" || name == "SYSTEM_" || name == "PUBLIC",
            member_of,
            details,
            ..user(&name)
        });
    }
    Ok(out)
}

fn altibase_kind(t: &str, table_type: &str) -> &'static str {
    match (t.trim(), table_type.trim()) {
        ("P", _) => kinds::PROCEDURE,
        ("S", _) => "sequence",
        (_, "V") => kinds::VIEW,
        _ => kinds::TABLE,
    }
}

pub async fn altibase_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let sys = rows(
        s,
        "SELECT u.USER_NAME AS GRANTEE, p.PRIV_NAME AS PRIVILEGE FROM SYSTEM_.SYS_GRANT_SYSTEM_ g
           JOIN SYSTEM_.SYS_PRIVILEGES_ p ON p.PRIV_ID = g.PRIV_ID JOIN SYSTEM_.SYS_USERS_ u ON u.USER_ID = g.GRANTEE_ID",
    )
    .await?;
    let obj = rows(
        s,
        "SELECT u.USER_NAME AS GRANTEE, p.PRIV_NAME AS PRIVILEGE, g.WITH_GRANT_OPTION AS GRANTABLE, g.OBJ_TYPE, o.USER_NAME AS OWNER,
                COALESCE(t.TABLE_NAME, pr.PROC_NAME) AS OBJ_NAME, t.TABLE_TYPE
           FROM SYSTEM_.SYS_GRANT_OBJECT_ g
           JOIN SYSTEM_.SYS_PRIVILEGES_ p ON p.PRIV_ID = g.PRIV_ID
           JOIN SYSTEM_.SYS_USERS_ u ON u.USER_ID = g.GRANTEE_ID
           JOIN SYSTEM_.SYS_USERS_ o ON o.USER_ID = g.USER_ID
           LEFT JOIN SYSTEM_.SYS_TABLES_ t ON t.TABLE_ID = g.OBJ_ID
           LEFT JOIN SYSTEM_.SYS_PROCEDURES_ pr ON pr.PROC_OID = g.OBJ_ID",
    )
    .await
    .unwrap_or_default();
    let members = members_map(&rows(s, ALTIBASE_MEMBERS).await.unwrap_or_default());
    Ok(with_roles(grantee(principal).1, &members, |n, via| {
        let mut out: Vec<Grant> = sys
            .iter()
            .filter(|r| get(r, "grantee") == n)
            .map(|r| Grant { privilege: get(r, "privilege").to_string(), via: via.clone(), ..Default::default() })
            .collect();
        for r in obj.iter().filter(|r| get(r, "grantee") == n) {
            out.push(Grant {
                privilege: get(r, "privilege").to_string(),
                object: Some(format!("{}.{}", get(r, "owner"), get(r, "obj_name"))),
                object_kind: Some(altibase_kind(get(r, "obj_type"), get(r, "table_type")).into()),
                grantable: super::yes(get(r, "grantable")),
                denied: false,
                via: via.clone(),
            });
        }
        out
    }))
}

// Dameng -----------------------------------------------------------------------

const DAMENG_MEMBERS: &str = "SELECT GRANTEE, GRANTED_ROLE, ADMIN_OPTION FROM DBA_ROLE_PRIVS";
const DAMENG_SYSTEM: &[&str] = &["SYSDBA", "SYSAUDITOR", "SYSSSO", "SYSDBO", "SYS", "PUBLIC", "DBA", "RESOURCE", "SOI", "SVI", "VTI"];

pub async fn dameng_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Dameng;
    let members = members_map(&rows(s, DAMENG_MEMBERS).await.unwrap_or_default());
    let mut out = Vec::new();
    for u in rows(s, "SELECT * FROM DBA_USERS").await? {
        let name = get(&u, "username").to_string();
        let member_of = members.get(&name.to_ascii_lowercase()).cloned().unwrap_or_default();
        let status = get(&u, "account_status").to_string();
        let mut details = Vec::new();
        for (k, label) in [("account_status", "Estado"), ("default_tablespace", "Espacio de tablas"), ("created", "Alta"), ("expiry_date", "Vence")] {
            let v = get(&u, k);
            if !v.is_empty() {
                details.push((label.to_string(), v.to_string()));
            }
        }
        out.push(Principal {
            superuser: Some(name == "SYSDBA" || member_of.iter().any(|r| r == "DBA")),
            disabled: Some(status.to_ascii_uppercase().contains("LOCK")),
            system: DAMENG_SYSTEM.contains(&name.as_str()),
            member_of,
            details,
            ..user(&name)
        });
    }
    let roles = match first(s, "SELECT NAME FROM SYSOBJECTS WHERE TYPE$ = 'UR' AND SUBTYPE$ = 'ROLE'").await {
        Ok(r) => r,
        Err(_) => first(s, "SELECT ROLE FROM DBA_ROLES").await.unwrap_or_default(),
    };
    for r in roles {
        out.push(Principal {
            member_of: members.get(&r.to_ascii_lowercase()).cloned().unwrap_or_default(),
            system: DAMENG_SYSTEM.contains(&r.as_str()) || r.starts_with("DB_"),
            superuser: Some(r == "DBA"),
            ..role(d, &r)
        });
    }
    Ok(out)
}

fn dameng_kind(t: &str) -> &'static str {
    match t.trim().to_ascii_uppercase().as_str() {
        "VIEW" => kinds::VIEW,
        "PROCEDURE" => kinds::PROCEDURE,
        "FUNCTION" => kinds::FUNCTION,
        "SEQUENCE" => "sequence",
        "SCHEMA" => "schema",
        _ => kinds::TABLE,
    }
}

pub async fn dameng_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let sys = rows(s, "SELECT GRANTEE, PRIVILEGE, ADMIN_OPTION FROM DBA_SYS_PRIVS").await?;
    let obj = rows(s, "SELECT * FROM DBA_TAB_PRIVS").await.unwrap_or_default();
    let members = members_map(&rows(s, DAMENG_MEMBERS).await.unwrap_or_default());
    Ok(with_roles(grantee(principal).1, &members, |n, via| {
        let mut out: Vec<Grant> = sys
            .iter()
            .filter(|r| get(r, "grantee") == n)
            .map(|r| Grant { privilege: get(r, "privilege").to_string(), grantable: super::yes(get(r, "admin_option")), via: via.clone(), ..Default::default() })
            .collect();
        for r in obj.iter().filter(|r| get(r, "grantee") == n) {
            out.push(Grant {
                privilege: get(r, "privilege").to_string(),
                object: Some(format!("{}.{}", get(r, "owner"), get(r, "table_name"))),
                object_kind: Some(dameng_kind(get(r, "type")).into()),
                grantable: super::yes(get(r, "grantable")),
                denied: false,
                via: via.clone(),
            });
        }
        out
    }))
}

// scripts ----------------------------------------------------------------------

/// Passwords in double quotes keep their case and special characters.
fn pw(p: &str) -> String {
    format!("\"{}\"", p.replace('"', "\"\""))
}

fn on(d: Dialect, o: &ObjectRef) -> String {
    format!(" ON {}", super::qualified(d, o))
}

pub fn script(d: Dialect, a: &SecurityAction) -> Result<String> {
    let id = |n: &str| ident(d, grantee(n).1);
    Ok(match a {
        SecurityAction::CreateUser { name, password: p } => format!("CREATE USER {} IDENTIFIED BY {};", id(name), pw(password(p)?)),
        SecurityAction::SetPassword { name, password: p } => format!("ALTER USER {} IDENTIFIED BY {};", id(name), pw(p)),
        SecurityAction::SetLogin { name, enabled } => format!("ALTER USER {} ACCOUNT {};", id(name), if *enabled { "UNLOCK" } else { "LOCK" }),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", id(name)),
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", id(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", id(name)),
        SecurityAction::Grant { privileges: p, object: None, to, grantable } => {
            if *grantable && d == Dialect::Altibase {
                return unsupported_action("Altibase no otorga privilegios del sistema con opción de otorgarlos a otros");
            }
            format!("GRANT {} TO {}{};", privileges(p)?, id(to), if *grantable { " WITH ADMIN OPTION" } else { "" })
        }
        SecurityAction::Grant { privileges: p, object: Some(o), to, grantable } => {
            format!("GRANT {}{} TO {}{};", privileges(p)?, on(d, o), id(to), option(*grantable))
        }
        SecurityAction::Revoke { privileges: p, object: None, from } => format!("REVOKE {} FROM {};", privileges(p)?, id(from)),
        SecurityAction::Revoke { privileges: p, object: Some(o), from } => format!("REVOKE {}{} FROM {};", privileges(p)?, on(d, o), id(from)),
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", id(role), id(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", id(role), id(member)),
    })
}


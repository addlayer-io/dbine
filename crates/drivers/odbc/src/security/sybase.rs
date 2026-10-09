//! SAP ASE and SAP SQL Anywhere.
//!
//! - **ASE**: the users and groups of the current database (`sysusers`),
//!   their logins (`master..syslogins`, bit 2 of `status`: locked), the
//!   server roles (`master..syssrvroles`, granted to logins in
//!   `master..sysloginroles`) and the permissions of the database
//!   (`sysprotects`, whose grantee is a user, a group or a role's local id
//!   from `sysroles`). Roles are named `role:<name>` because membership
//!   differs: `grant role` for a role, `sp_changegroup` for a group.
//! - **SQL Anywhere** (16 and later): users and roles in `SYS.SYSUSER`
//!   (`user_type` 12 user, 13 role, 14 user extended as role, 1/5/9 system
//!   roles), memberships in `SYS.SYSROLEGRANTS` (system privileges are
//!   grants of `SYS_…_ROLE` roles) and table permissions in
//!   `SYS.SYSTABAUTH`.

use super::{db2_auths, get, grantee, lit, option, password, privileges, rows, unsupported_action, user, with_roles, Dialect, Row};
use crate::OdbcSession;
use dbine_driver::{kinds, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashMap;

pub fn spec_ase() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "EXECUTE", "TRUNCATE TABLE", "UPDATE STATISTICS", "DELETE STATISTICS",
            "CREATE TABLE", "CREATE VIEW", "CREATE PROCEDURE", "CREATE FUNCTION", "CREATE DEFAULT", "CREATE RULE", "CREATE TRIGGER",
            "DUMP DATABASE", "DUMP TRANSACTION", "ALL",
        ],
        // "" = the database (command permissions).
        object_kinds: vec!["", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: true,
    }
}

pub fn spec_sa() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "REFERENCES", "LOAD", "TRUNCATE", "EXECUTE", "ALL", "CREATE TABLE",
            "CREATE ANY TABLE", "CREATE VIEW", "CREATE ANY VIEW", "CREATE PROCEDURE", "CREATE ANY PROCEDURE", "SELECT ANY TABLE",
            "BACKUP DATABASE", "MANAGE ANY USER", "MONITOR", "SERVER OPERATOR",
        ],
        // "" = system privileges.
        object_kinds: vec!["", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

// ASE --------------------------------------------------------------------------

/// `sysprotects.action`.
pub(super) fn ase_action(code: &str) -> String {
    match code.trim() {
        "151" => "REFERENCES",
        "167" => "SET PROXY",
        "193" => "SELECT",
        "195" => "INSERT",
        "196" => "DELETE",
        "197" => "UPDATE",
        "198" => "CREATE TABLE",
        "203" => "CREATE DATABASE",
        "207" => "CREATE VIEW",
        "221" => "CREATE TRIGGER",
        "222" => "CREATE PROCEDURE",
        "224" => "EXECUTE",
        "228" => "DUMP DATABASE",
        "233" => "CREATE DEFAULT",
        "235" => "DUMP TRANSACTION",
        "236" => "CREATE RULE",
        "253" => "CONNECT",
        "280" => "CREATE FUNCTION",
        "282" => "DELETE STATISTICS",
        "320" => "TRUNCATE TABLE",
        "326" => "UPDATE STATISTICS",
        "347" => "SET TRACING",
        "353" => "DECRYPT",
        "354" => "CREATE ENCRYPTION KEY",
        "368" => "TRANSFER TABLE",
        other => return format!("ACCIÓN {other}"),
    }
    .to_string()
}

/// A group of `sysusers`: `public` or one with its own uid as gid, from
/// @@mingroupid (16384) up.
pub(super) fn ase_group(r: &Row) -> bool {
    let (uid, gid) = (get(r, "uid"), get(r, "gid"));
    let n: i64 = uid.parse().unwrap_or(-1);
    uid == gid && (n == 0 || n >= 16384)
}

const ASE_USERS: &str = "SELECT u.uid, u.gid, u.suid, u.name, l.name AS login_name, l.status AS login_status, l.dbname AS default_db
  FROM sysusers u LEFT JOIN master..syslogins l ON l.suid = u.suid";

const ASE_ROLE_LOGINS: &str = "SELECT lr.suid, r.name AS role_name FROM master..sysloginroles lr JOIN master..syssrvroles r ON r.srid = lr.srid";

pub async fn ase_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Ase;
    let users = rows(s, ASE_USERS).await?;
    let roles = rows(s, "SELECT srid, name FROM master..syssrvroles").await.unwrap_or_default();
    let grants = rows(s, ASE_ROLE_LOGINS).await.unwrap_or_default();
    let groups: HashMap<String, String> = users.iter().filter(|u| ase_group(u)).map(|u| (get(u, "uid").to_string(), get(u, "name").to_string())).collect();
    let mut out = Vec::new();
    for u in users.iter().filter(|u| !ase_group(u)) {
        let name = get(u, "name").to_string();
        let suid = get(u, "suid");
        let mut member_of: Vec<String> = groups.get(get(u, "gid")).filter(|g| *g != "public").cloned().into_iter().collect();
        let my_roles: Vec<String> = grants.iter().filter(|g| get(g, "suid") == suid).map(|g| get(g, "role_name").to_string()).collect();
        member_of.extend(my_roles.iter().map(|r| super::role_name(d, r)));
        let mut details = Vec::new();
        if let Some(l) = Some(get(u, "login_name")).filter(|l| !l.is_empty() && *l != name) {
            details.push(("Login".to_string(), l.to_string()));
        }
        if !get(u, "default_db").is_empty() {
            details.push(("Base predeterminada".into(), get(u, "default_db").to_string()));
        }
        let status: i64 = get(u, "login_status").parse().unwrap_or(0);
        out.push(Principal {
            superuser: Some(name == "dbo" || my_roles.iter().any(|r| r == "sa_role")),
            disabled: Some(status & 2 != 0),
            can_login: Some(!get(u, "login_name").is_empty() || name == "guest"),
            system: matches!(name.as_str(), "dbo" | "guest"),
            member_of,
            details,
            ..user(&name)
        });
    }
    for name in groups.values() {
        out.push(Principal {
            name: name.clone(),
            kind: PrincipalKind::Role,
            can_login: Some(false),
            system: name == "public",
            details: vec![("Tipo".into(), "grupo de la base".into())],
            ..Default::default()
        });
    }
    for r in &roles {
        let name = get(r, "name");
        let srid: i64 = get(r, "srid").parse().unwrap_or(99);
        out.push(Principal {
            superuser: Some(name == "sa_role"),
            system: srid < 32 && name.ends_with("_role"),
            details: vec![("Tipo".into(), "rol del servidor".into())],
            ..super::role(d, name)
        });
    }
    Ok(out)
}

fn ase_kind(t: &str) -> &'static str {
    match t.trim() {
        "V" => kinds::VIEW,
        "P" | "XP" => kinds::PROCEDURE,
        "SF" | "F" => kinds::FUNCTION,
        _ => kinds::TABLE,
    }
}

pub(super) fn ase_grant(r: &Row, via: Option<String>) -> Grant {
    let obj = get(r, "objname");
    let object = (get(r, "id") != "0" && !obj.is_empty()).then(|| {
        let owner = get(r, "owner");
        if owner.is_empty() {
            obj.to_string()
        } else {
            format!("{owner}.{obj}")
        }
    });
    let kind = object.as_ref().map(|_| ase_kind(get(r, "objtype")).to_string());
    let pt = get(r, "protecttype");
    Grant { privilege: ase_action(get(r, "action")), object, object_kind: kind, grantable: pt == "0", denied: pt == "2", via }
}

const ASE_PROTECTS: &str = "SELECT p.id, p.action, p.protecttype, o.name AS objname, USER_NAME(o.uid) AS owner, o.type AS objtype
  FROM sysprotects p LEFT JOIN sysobjects o ON o.id = p.id WHERE ";

pub async fn ase_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let (is_role, name) = grantee(principal);
    let role_uid = |r: &str| {
        format!("p.uid IN (SELECT lr.lrid FROM sysroles lr, master..syssrvroles sr WHERE sr.srid = lr.id AND sr.name = {})", lit(r))
    };
    // (condition, via)
    let mut who: Vec<(String, Option<String>)> = Vec::new();
    if is_role {
        who.push((role_uid(name), None));
    } else {
        let users = rows(s, ASE_USERS).await?;
        let me = users.iter().find(|u| get(u, "name") == name);
        match me {
            Some(u) => {
                who.push((format!("p.uid = {}", get(u, "uid").parse::<i64>().unwrap_or(-1)), None));
                if !ase_group(u) {
                    if let Some(g) = users.iter().find(|g| ase_group(g) && get(g, "uid") == get(u, "gid") && get(g, "uid") != "0") {
                        who.push((format!("p.uid = {}", get(g, "uid").parse::<i64>().unwrap_or(-1)), Some(get(g, "name").to_string())));
                    }
                    let suid = get(u, "suid");
                    for g in rows(s, ASE_ROLE_LOGINS).await.unwrap_or_default().iter().filter(|g| get(g, "suid") == suid) {
                        let r = get(g, "role_name");
                        who.push((role_uid(r), Some(super::role_name(Dialect::Ase, r))));
                    }
                }
            }
            None => who.push((format!("p.uid = USER_ID({})", lit(name)), None)),
        }
    }
    let mut out = Vec::new();
    for (cond, via) in who {
        match rows(s, &format!("{ASE_PROTECTS}{cond}")).await {
            Ok(rs) => out.extend(rs.iter().map(|r| ase_grant(r, via.clone()))),
            Err(e) if via.is_none() => return Err(e),
            Err(_) => {}
        }
    }
    Ok(out)
}

/// ASE takes brackets around any name.
fn ase_ident(n: &str) -> String {
    super::ident(Dialect::Ase, n)
}

fn ase_on(o: &ObjectRef) -> String {
    match o.schema().filter(|s| !s.is_empty()) {
        Some(owner) => format!(" on {}.{}", ase_ident(owner), ase_ident(&o.name)),
        None => format!(" on {}", ase_ident(&o.name)),
    }
}

pub fn ase_script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => {
            let pw = password(pw)?;
            format!("exec sp_addlogin {n}, {}\nexec sp_adduser {n}", lit(pw), n = lit(name))
        }
        SecurityAction::SetPassword { .. } => {
            return unsupported_action(
                "ASE pide la contraseña de quien hace el cambio (alter login … with password <la tuya> modify password …): cambiala desde un editor de SQL",
            )
        }
        SecurityAction::SetLogin { name, enabled } => format!("exec sp_locklogin {}, '{}'", lit(name), if *enabled { "unlock" } else { "lock" }),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("exec sp_dropuser {}", lit(name)),
        SecurityAction::CreateRole { name } => format!("create role {}", ase_ident(grantee(name).1)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => match grantee(name) {
            (true, r) => format!("drop role {}", ase_ident(r)),
            (false, g) => format!("exec sp_dropgroup {}", lit(g)),
        },
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            let on = object.as_ref().map(ase_on).unwrap_or_default();
            format!("grant {}{on} to {}{}", privileges(p)?.to_lowercase(), ase_ident(grantee(to).1), option(*grantable).to_lowercase())
        }
        SecurityAction::Revoke { privileges: p, object, from } => {
            let on = object.as_ref().map(ase_on).unwrap_or_default();
            format!("revoke {}{on} from {}", privileges(p)?.to_lowercase(), ase_ident(grantee(from).1))
        }
        SecurityAction::AddMember { role, member } => match grantee(role) {
            (true, r) => format!("grant role {} to {}", ase_ident(r), ase_ident(grantee(member).1)),
            (false, g) => format!("exec sp_changegroup {}, {}", lit(g), lit(member)),
        },
        SecurityAction::RemoveMember { role, member } => match grantee(role) {
            (true, r) => format!("revoke role {} from {}", ase_ident(r), ase_ident(grantee(member).1)),
            // A user leaves its group by going back to public.
            (false, _) => format!("exec sp_changegroup 'public', {}", lit(member)),
        },
    })
}

/// "Asignar login…": `sp_adduser loginame [, name_in_db [, grpname]]`, the
/// login first. ASE users have no default schema (objects are found by
/// owner: the user, then dbo), so there's none to set.
pub fn ase_map_login(login: &str, user: &str) -> Result<String> {
    let (login, user) = (login.trim(), user.trim());
    if login.is_empty() {
        return Err(dbine_driver::Error::Query("elegí o escribí el login".into()));
    }
    if user.is_empty() {
        return Err(dbine_driver::Error::Query("escribí el nombre del usuario".into()));
    }
    Ok(format!("exec sp_adduser {}, {}", lit(login), lit(user)))
}

/// The server's logins with no user in the current database: neither a
/// user of `sysusers` nor an alias (`sysalternates`, `sp_addalias`), which
/// already lets the login in as another user.
pub const ASE_UNMAPPED_LOGINS: &str = "SELECT l.name FROM master..syslogins l
 WHERE l.suid NOT IN (SELECT u.suid FROM sysusers u WHERE u.suid IS NOT NULL)
   AND l.suid NOT IN (SELECT a.suid FROM sysalternates a)
 ORDER BY l.name";

pub async fn ase_unmapped_logins(s: &OdbcSession) -> Result<Vec<String>> {
    Ok(rows(s, ASE_UNMAPPED_LOGINS).await?.iter().map(|r| get(r, "name").to_string()).filter(|n| !n.is_empty()).collect())
}

// SQL Anywhere ---------------------------------------------------------------

/// `SYS_CREATE_ANY_TABLE_ROLE` → `CREATE ANY TABLE`: the system privilege
/// behind a system role, if it's one.
pub(super) fn sa_privilege(role: &str) -> Option<String> {
    let inner = role.strip_prefix("SYS_")?.strip_suffix("_ROLE")?;
    if inner.starts_with("AUTH_") || inner.starts_with("RUN_") || inner.starts_with("REPLICATION") {
        return None;
    }
    Some(inner.replace('_', " "))
}

async fn sa_members(s: &OdbcSession) -> Vec<Row> {
    rows(s, "SELECT role_name, grantee_name, grant_type FROM SYS.SYSROLEGRANTS").await.unwrap_or_default()
}

pub async fn sa_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::SqlAnywhere;
    let users = rows(
        s,
        "SELECT user_name, user_type, lock_time, failed_login_attempts, last_login_time,
                CASE WHEN password IS NULL THEN 0 ELSE 1 END AS has_password
           FROM SYS.SYSUSER",
    )
    .await?;
    let members = sa_members(s).await;
    let mut out: Vec<Principal> = Vec::new();
    for u in &users {
        let name = get(u, "user_name").to_string();
        let ty = get(u, "user_type");
        let member_of: Vec<String> = members.iter().filter(|m| get(m, "grantee_name") == name).map(|m| get(m, "role_name").to_string()).collect();
        let system = matches!(ty, "1" | "5" | "9") || name.starts_with("SYS_") || matches!(name.as_str(), "SYS" | "PUBLIC" | "dbo" | "diagnostics" | "rs_systabgroup" | "SA_DEBUG");
        if matches!(ty, "12" | "14") {
            let mut details = Vec::new();
            if ty == "14" {
                details.push(("Tipo".to_string(), "usuario que también es rol".to_string()));
            }
            for (k, label) in [("last_login_time", "Último ingreso"), ("lock_time", "Bloqueado desde"), ("failed_login_attempts", "Intentos fallidos")] {
                let v = get(u, k);
                if !v.is_empty() && v != "0" {
                    details.push((label.to_string(), v.to_string()));
                }
            }
            out.push(Principal {
                superuser: Some(member_of.iter().any(|r| r == "SYS_AUTH_DBA_ROLE" || r == "SYS_AUTH_SSO_ROLE")),
                can_login: Some(get(u, "has_password") == "1"),
                disabled: Some(!get(u, "lock_time").is_empty()),
                system,
                member_of,
                details,
                ..user(&name)
            });
        } else {
            // System privileges are roles too: they're listed as grants.
            if sa_privilege(&name).is_some() && matches!(ty, "1" | "5" | "9") {
                continue;
            }
            out.push(Principal { member_of, system, superuser: Some(name == "SYS_AUTH_DBA_ROLE"), ..super::role(d, &name) });
        }
    }
    Ok(out)
}

pub async fn sa_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let tabs = rows(
        s,
        "SELECT a.*, t.table_type AS objtype FROM SYS.SYSTABAUTH a
           LEFT JOIN SYS.SYSTABLE t ON t.table_name = a.ttname AND USER_NAME(t.creator) = a.tcreator",
    )
    .await?;
    let procs = rows(
        s,
        "SELECT USER_NAME(pp.grantee) AS grantee, p.proc_name, USER_NAME(p.creator) AS owner
           FROM SYS.SYSPROCPERM pp JOIN SYS.SYSPROCEDURE p ON p.proc_id = pp.proc_id",
    )
    .await
    .unwrap_or_default();
    let all = sa_members(s).await;
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for m in &all {
        if sa_privilege(get(m, "role_name")).is_none() {
            members.entry(get(m, "grantee_name").to_ascii_lowercase()).or_default().push(get(m, "role_name").to_string());
        }
    }
    Ok(with_roles(principal, &members, |n, via| {
        let mut out = Vec::new();
        for m in all.iter().filter(|m| get(m, "grantee_name") == n) {
            if let Some(p) = sa_privilege(get(m, "role_name")) {
                // grant_type, right to left: granted, admin, inheritable.
                let admin = get(m, "grant_type").chars().rev().nth(1) == Some('1');
                out.push(Grant { privilege: p, grantable: admin, via: via.clone(), ..Default::default() });
            }
        }
        for r in tabs.iter().filter(|r| get(r, "grantee") == n) {
            let kind = if get(r, "objtype").eq_ignore_ascii_case("VIEW") { kinds::VIEW } else { kinds::TABLE };
            out.extend(db2_auths(r, Some(format!("{}.{}", get(r, "tcreator"), get(r, "ttname"))), Some(kind), &via));
        }
        for p in procs.iter().filter(|p| get(p, "grantee") == n) {
            out.push(Grant {
                privilege: "EXECUTE".into(),
                object: Some(format!("{}.{}", get(p, "owner"), get(p, "proc_name"))),
                object_kind: Some(kinds::PROCEDURE.into()),
                via: via.clone(),
                ..Default::default()
            });
        }
        out
    }))
}

fn sa_ident(n: &str) -> String {
    super::ident(Dialect::SqlAnywhere, n)
}

/// SQL Anywhere's passwords are identifiers: in double quotes they keep
/// their case and any character.
fn sa_password(p: &str) -> String {
    format!("\"{}\"", p.replace('"', "\"\""))
}

pub fn sa_script(a: &SecurityAction) -> Result<String> {
    let d = Dialect::SqlAnywhere;
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => format!("CREATE USER {} IDENTIFIED BY {};", sa_ident(name), sa_password(password(pw)?)),
        SecurityAction::SetPassword { name, password: pw } => format!("ALTER USER {} IDENTIFIED BY {};", sa_ident(name), sa_password(pw)),
        SecurityAction::SetLogin { name, enabled: true } => format!("ALTER USER {} RESET LOGIN POLICY;", sa_ident(name)),
        SecurityAction::SetLogin { enabled: false, .. } => {
            return unsupported_action(
                "SQL Anywhere bloquea usuarios con una política de ingreso (CREATE LOGIN POLICY … LOCKED=ON y ALTER USER … LOGIN POLICY): asignásela desde un editor de SQL",
            )
        }
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", sa_ident(name)),
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", sa_ident(grantee(name).1)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", sa_ident(grantee(name).1)),
        SecurityAction::Grant { privileges: p, object: None, to, grantable } => {
            format!("GRANT {} TO {}{};", privileges(p)?, sa_ident(grantee(to).1), if *grantable { " WITH ADMIN OPTION" } else { "" })
        }
        SecurityAction::Grant { privileges: p, object: Some(o), to, grantable } => {
            format!("GRANT {} ON {} TO {}{};", privileges(p)?, super::qualified(d, o), sa_ident(grantee(to).1), option(*grantable))
        }
        SecurityAction::Revoke { privileges: p, object: None, from } => format!("REVOKE {} FROM {};", privileges(p)?, sa_ident(grantee(from).1)),
        SecurityAction::Revoke { privileges: p, object: Some(o), from } => {
            format!("REVOKE {} ON {} FROM {};", privileges(p)?, super::qualified(d, o), sa_ident(grantee(from).1))
        }
        SecurityAction::AddMember { role, member } => format!("GRANT ROLE {} TO {};", sa_ident(grantee(role).1), sa_ident(grantee(member).1)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE ROLE {} FROM {};", sa_ident(grantee(role).1), sa_ident(grantee(member).1)),
    })
}


//! Teradata: users (`DBC.UsersV`), roles (`DBC.RoleInfoV`), their members
//! (`DBC.RoleMembersV`) and rights (`DBC.AllRightsV` for users,
//! `DBC.AllRoleRightsV` for roles). A Teradata "database" is what DBine
//! shows as a schema.

use super::{add_member, first, get, ident, lit, privileges, role, rows, user, Dialect, Row};
use crate::OdbcSession;
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::{HashSet, VecDeque};

const D: Dialect = Dialect::Teradata;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "INDEX", "EXECUTE", "EXECUTE PROCEDURE", "EXECUTE FUNCTION",
            "CREATE TABLE", "CREATE VIEW", "CREATE MACRO", "CREATE PROCEDURE", "CREATE FUNCTION", "DROP TABLE", "DROP VIEW",
            "DROP MACRO", "DROP PROCEDURE", "DROP FUNCTION", "SHOW", "STATISTICS", "DUMP", "RESTORE", "ALL",
        ],
        object_kinds: vec!["schema", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

/// `AccessRight` codes of the rights views.
pub(super) fn right(code: &str) -> String {
    match code.trim().to_ascii_uppercase().as_str() {
        "R" => "SELECT",
        "I" => "INSERT",
        "U" => "UPDATE",
        "D" => "DELETE",
        "RF" => "REFERENCES",
        "IX" => "INDEX",
        "E" => "EXECUTE",
        "PE" => "EXECUTE PROCEDURE",
        "EF" => "EXECUTE FUNCTION",
        "CT" => "CREATE TABLE",
        "CV" => "CREATE VIEW",
        "CM" => "CREATE MACRO",
        "PC" => "CREATE PROCEDURE",
        "CF" => "CREATE FUNCTION",
        "CG" => "CREATE TRIGGER",
        "CD" => "CREATE DATABASE",
        "CU" => "CREATE USER",
        "CR" => "CREATE ROLE",
        "CO" => "CREATE PROFILE",
        "CE" => "CREATE EXTERNAL PROCEDURE",
        "CA" => "CREATE AUTHORIZATION",
        "DT" => "DROP TABLE",
        "DV" => "DROP VIEW",
        "DM" => "DROP MACRO",
        "PD" => "DROP PROCEDURE",
        "DF" => "DROP FUNCTION",
        "DG" => "DROP TRIGGER",
        "DD" => "DROP DATABASE",
        "DU" => "DROP USER",
        "DR" => "DROP ROLE",
        "DO" => "DROP PROFILE",
        "DA" => "DROP AUTHORIZATION",
        "AP" => "ALTER PROCEDURE",
        "AF" => "ALTER FUNCTION",
        "AE" => "ALTER EXTERNAL PROCEDURE",
        "AS" => "ABORT SESSION",
        "DP" => "DUMP",
        "RS" => "RESTORE",
        "CP" => "CHECKPOINT",
        "SH" => "SHOW",
        "ST" => "STATISTICS",
        "MR" => "MONITOR RESOURCE",
        "MS" => "MONITOR SESSION",
        "SR" => "SET RESOURCE RATE",
        "SS" => "SET SESSION RATE",
        "UT" => "UDT TYPE",
        "UU" => "UDT USAGE",
        "UM" => "UDT METHOD",
        "NT" => "NONTEMPORAL",
        "RO" => "REPLCONTROL",
        other => return other.to_string(),
    }
    .to_string()
}

const SYSTEM_USERS: &[&str] = &["DBC", "SYSTEMFE", "SYSADMIN", "TDWM", "SYSLIB", "SYSUDTLIB", "SYSSPATIAL", "TD_SYSFNLIB", "TDPUSER", "SYSDBA", "PUBLIC"];

pub async fn principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let mut out: Vec<Principal> = Vec::new();
    for u in rows(s, "SELECT * FROM DBC.UsersV").await? {
        let name = get(&u, "username").to_string();
        let mut details = Vec::new();
        for (k, label) in [
            ("defaultdatabase", "Base predeterminada"),
            ("profilename", "Perfil"),
            ("defaultrole", "Rol predeterminado"),
            ("createtimestamp", "Alta"),
            ("passwordlastmoddate", "Contraseña cambiada"),
            ("lockeddate", "Bloqueado el"),
            ("lockedcount", "Intentos fallidos"),
        ] {
            let v = get(&u, k);
            if !v.is_empty() && !(k == "lockedcount" && v == "0") {
                details.push((label.to_string(), v.to_string()));
            }
        }
        out.push(Principal {
            superuser: Some(name.eq_ignore_ascii_case("DBC")),
            system: SYSTEM_USERS.contains(&name.to_ascii_uppercase().as_str()),
            details,
            ..user(&name)
        });
    }
    let roles: Vec<String> = first(s, "SELECT RoleName FROM DBC.RoleInfoV").await.unwrap_or_default();
    for r in &roles {
        out.push(role(D, r));
    }
    let known: HashSet<String> = roles.iter().map(|r| r.to_ascii_uppercase()).collect();
    for m in rows(s, "SELECT * FROM DBC.RoleMembersV").await.unwrap_or_default() {
        let (r, grantee) = (get(&m, "rolename").to_string(), get(&m, "grantee").to_string());
        let is_role = match get(&m, "granteekind").to_ascii_uppercase().as_str() {
            "R" => true,
            "U" => false,
            _ => known.contains(&grantee.to_ascii_uppercase()),
        };
        let member = if is_role { role(D, &grantee) } else { user(&grantee) };
        add_member(&mut out, member, r);
    }
    Ok(out)
}

fn object_kind(k: &str) -> &'static str {
    match k.trim().to_ascii_uppercase().as_str() {
        "V" => kinds::VIEW,
        "P" | "E" => kinds::PROCEDURE,
        "F" | "A" | "B" | "R" | "S" => kinds::FUNCTION,
        "M" => "macro",
        _ => kinds::TABLE,
    }
}

/// A row of `AllRightsV` / `AllRoleRightsV` (joined with `TablesV`'s kind).
pub(super) fn grant_of(r: &Row, via: Option<String>) -> Option<Grant> {
    let code = get(r, "accessright");
    if code.is_empty() {
        return None;
    }
    let (db, table) = (get(r, "databasename"), get(r, "tablename"));
    let (object, kind) = if table.is_empty() || table.eq_ignore_ascii_case("All") {
        (db.to_string(), "schema")
    } else {
        (format!("{db}.{table}"), object_kind(get(r, "tablekind")))
    };
    Some(Grant {
        privilege: right(code),
        object: Some(object),
        object_kind: Some(kind.into()),
        grantable: get(r, "grantauthority").eq_ignore_ascii_case("Y"),
        denied: false,
        via,
    })
}

pub async fn grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let roles: HashSet<String> = first(s, "SELECT RoleName FROM DBC.RoleInfoV").await.unwrap_or_default().into_iter().map(|r| r.to_ascii_uppercase()).collect();
    let members = rows(s, "SELECT * FROM DBC.RoleMembersV").await.unwrap_or_default();
    let name = super::grantee(principal).1;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([(name.to_string(), roles.contains(&name.to_ascii_uppercase()), None::<String>)]);
    while let Some((n, is_role, via)) = queue.pop_front() {
        if !seen.insert(n.to_ascii_uppercase()) || seen.len() > 64 {
            continue;
        }
        let (view, key) = if is_role { ("DBC.AllRoleRightsV", "RoleName") } else { ("DBC.AllRightsV", "UserName") };
        let sql = format!(
            "SELECT r.*, t.TableKind FROM {view} r LEFT JOIN DBC.TablesV t ON t.DatabaseName = r.DatabaseName AND t.TableName = r.TableName WHERE r.{key} = {}",
            lit(&n)
        );
        match rows(s, &sql).await {
            Ok(rs) => out.extend(rs.iter().filter_map(|r| grant_of(r, via.clone()))),
            Err(e) if via.is_none() => return Err(e),
            Err(_) => {}
        }
        for m in members.iter().filter(|m| get(m, "grantee").eq_ignore_ascii_case(&n)) {
            let r = get(m, "rolename").to_string();
            let v = via.clone().unwrap_or_else(|| r.clone());
            queue.push_back((r, true, Some(v)));
        }
    }
    Ok(out)
}

/// Teradata passwords go in double quotes (special characters allowed).
fn password(p: &str) -> String {
    format!("\"{}\"", p.replace('"', "\"\""))
}

fn on(o: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = o else {
        return Err(Error::Query("elegí una base de datos, una tabla o una vista: Teradata no tiene permisos sobre todo el servidor".into()));
    };
    Ok(match o.kind.as_str() {
        "schema" | "database" => format!(" ON {}", ident(D, &o.name)),
        k if k == kinds::PROCEDURE => format!(" ON PROCEDURE {}", super::qualified(D, o)),
        k if k == kinds::FUNCTION => format!(" ON FUNCTION {}", super::qualified(D, o)),
        _ => format!(" ON {}", super::qualified(D, o)),
    })
}

pub fn script(a: &SecurityAction) -> Result<String> {
    let id = |n: &str| ident(D, super::grantee(n).1);
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => {
            let pw = pw.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} AS PERMANENT = 0, PASSWORD = {};", id(name), password(pw))
        }
        SecurityAction::SetPassword { name, password: pw } => format!("MODIFY USER {} AS PASSWORD = {};", id(name), password(pw)),
        SecurityAction::SetLogin { name, enabled: true } => {
            format!("MODIFY USER {n} AS RELEASE PASSWORD LOCK;\nGRANT LOGON ON ALL TO {n};", n = id(name))
        }
        SecurityAction::SetLogin { name, enabled: false } => format!("REVOKE LOGON ON ALL FROM {};", id(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", id(name)),
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", id(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", id(name)),
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            format!("GRANT {}{} TO {}{};", privileges(p)?, on(object)?, id(to), if *grantable { " WITH GRANT OPTION" } else { "" })
        }
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {}{} FROM {};", privileges(p)?, on(object)?, id(from)),
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", id(role), id(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", id(role), id(member)),
    })
}

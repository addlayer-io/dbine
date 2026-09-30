//! IBM Netezza: users (`_V_USER`), groups (`_V_GROUP`, members in
//! `_V_GROUPUSERS`) and the object privileges of users and groups
//! (`_T_USROBJ_PRIV`, `_T_GRPOBJ_PRIV`: a bit mask per object, another for
//! the grantable ones; object 0 holds the administration privileges).
//! Netezza has groups instead of roles, and GRANT names them
//! (`TO GROUP g`), so DBine lists them as `role:<name>`.

use super::{get, grantee, ident, lit, option, password, privileges, role, rows, unsupported_action, user, Dialect, Row};
use crate::OdbcSession;
use dbine_driver::{kinds, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashMap;

const D: Dialect = Dialect::Netezza;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "LIST", "SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "LOCK", "ALTER", "DROP", "ABORT", "LOAD", "GENSTATS", "GROOM",
            "EXECUTE", "ALL", "CREATE TABLE", "CREATE VIEW", "CREATE EXTERNAL TABLE", "CREATE SEQUENCE", "CREATE PROCEDURE",
            "CREATE DATABASE", "CREATE USER", "CREATE GROUP", "BACKUP", "RESTORE",
        ],
        // "" = administration privileges.
        object_kinds: vec!["", kinds::TABLE, kinds::VIEW],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

pub async fn principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let members = rows(s, "SELECT GROUPNAME, USERNAME FROM _V_GROUPUSERS").await.unwrap_or_default();
    let of = |n: &str| -> Vec<String> {
        members.iter().filter(|m| get(m, "username").eq_ignore_ascii_case(n)).map(|m| super::role_name(D, get(m, "groupname"))).collect()
    };
    let mut out = Vec::new();
    for u in rows(s, "SELECT * FROM _V_USER").await? {
        let name = get(&u, "username").to_string();
        let mut details = Vec::new();
        for (k, label) in [("valuntil", "Vence"), ("useauth", "Autenticación"), ("rowlimit", "Límite de filas"), ("pwd_last_chged", "Contraseña cambiada")] {
            let v = get(&u, k);
            if !v.is_empty() && v != "0" {
                details.push((label.to_string(), v.to_string()));
            }
        }
        let locked = ["account_locked", "locked"].iter().map(|k| get(&u, k)).find(|v| !v.is_empty());
        out.push(Principal {
            superuser: Some(name.eq_ignore_ascii_case("ADMIN")),
            disabled: locked.map(super::yes),
            system: name.eq_ignore_ascii_case("ADMIN"),
            member_of: of(&name),
            details,
            ..user(&name)
        });
    }
    for g in rows(s, "SELECT GROUPNAME FROM _V_GROUP").await.unwrap_or_default() {
        let name = get(&g, "groupname");
        out.push(Principal { system: name.eq_ignore_ascii_case("PUBLIC"), ..role(D, name) });
    }
    Ok(out)
}

/// The object privilege bits.
const BITS: &[(u64, &str)] = &[
    (1, "LIST"),
    (2, "SELECT"),
    (4, "INSERT"),
    (8, "UPDATE"),
    (16, "DELETE"),
    (32, "TRUNCATE"),
    (64, "LOCK"),
    (128, "ALTER"),
    (256, "DROP"),
    (512, "ABORT"),
    (2048, "LOAD"),
    (4096, "GENSTATS"),
    (8192, "EXECUTE"),
    (16384, "GROOM"),
];

fn kind(t: &str) -> &'static str {
    match t.trim().to_ascii_uppercase().as_str() {
        "VIEW" | "MATERIALIZED VIEW" => kinds::VIEW,
        "PROCEDURE" => kinds::PROCEDURE,
        "FUNCTION" | "AGGREGATE" => kinds::FUNCTION,
        "DATABASE" => "database",
        "SCHEMA" => "schema",
        "SEQUENCE" => "sequence",
        _ => kinds::TABLE,
    }
}

/// One row of the privilege tables (joined with the object's name).
pub(super) fn grants_of(r: &Row, via: Option<String>) -> Vec<Grant> {
    let mask: u64 = get(r, "priv").parse().unwrap_or(0);
    let gmask: u64 = get(r, "gpriv").parse().unwrap_or(0);
    let object_id = get(r, "objid");
    let admin = object_id == "0" || object_id.is_empty();
    let (object, object_kind) = if admin {
        (None, None)
    } else {
        let name = get(r, "objname");
        let name = if name.is_empty() { format!("objeto {object_id}") } else { name.to_string() };
        let db = get(r, "dbname");
        let schema = get(r, "objschema");
        let full = [db, schema, name.as_str()].iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join(".");
        (Some(full), Some(kind(get(r, "objtype")).to_string()))
    };
    if admin {
        // The administration privileges' bits aren't the objects' ones.
        return (mask != 0)
            .then(|| Grant { privilege: format!("ADMINISTRACIÓN (máscara {mask})"), grantable: gmask != 0, via: via.clone(), ..Default::default() })
            .into_iter()
            .collect();
    }
    BITS.iter()
        .filter(|(b, _)| mask & b != 0)
        .map(|(b, p)| Grant { privilege: p.to_string(), object: object.clone(), object_kind: object_kind.clone(), grantable: gmask & b != 0, denied: false, via: via.clone() })
        .collect()
}

fn privs_sql(is_group: bool, name: &str) -> String {
    let (t, p, who, list, key) = if is_group {
        ("_T_GRPOBJ_PRIV", "GOP", "GOPGROUP", "_V_GROUP", "GROUPNAME")
    } else {
        ("_T_USROBJ_PRIV", "UOP", "UOPUSER", "_V_USER", "USERNAME")
    };
    format!(
        "SELECT x.{p}OBJECT AS OBJID, x.{p}OBJPRIV AS PRIV, x.{p}GOBJPRIV AS GPRIV, o.OBJNAME, o.OBJTYPE, o.SCHEMA AS OBJSCHEMA, d.DATABASE AS DBNAME
   FROM {t} x LEFT JOIN _V_OBJECT_DATA o ON o.OBJID = x.{p}OBJECT LEFT JOIN _V_DATABASE d ON d.OBJID = x.{p}DB
  WHERE x.{who} = (SELECT OBJID FROM {list} WHERE {key} = {})",
        lit(name)
    )
}

pub async fn grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let (is_group, name) = grantee(principal);
    let mut out = Vec::new();
    for r in rows(s, &privs_sql(is_group, name)).await? {
        out.extend(grants_of(&r, None));
    }
    if !is_group {
        let groups: HashMap<String, ()> = rows(s, &format!("SELECT GROUPNAME FROM _V_GROUPUSERS WHERE USERNAME = {}", lit(name)))
            .await
            .unwrap_or_default()
            .iter()
            .map(|g| (get(g, "groupname").to_string(), ()))
            .collect();
        for g in groups.keys() {
            for r in rows(s, &privs_sql(true, g)).await.unwrap_or_default() {
                out.extend(grants_of(&r, Some(super::role_name(D, g))));
            }
        }
    }
    Ok(out)
}

fn to_whom(n: &str) -> String {
    match grantee(n) {
        (true, g) => format!("GROUP {}", ident(D, g)),
        (false, u) => ident(D, u),
    }
}

fn on(o: &Option<ObjectRef>) -> String {
    o.as_ref().map(|o| format!(" ON {}", super::qualified(D, o))).unwrap_or_default()
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => format!("CREATE USER {} WITH PASSWORD {};", ident(D, name), lit(password(pw)?)),
        SecurityAction::SetPassword { name, password: pw } => format!("ALTER USER {} WITH PASSWORD {};", ident(D, name), lit(pw)),
        SecurityAction::SetLogin { name, enabled: true } => format!("ALTER USER {} RESET ACCOUNT;", ident(D, name)),
        SecurityAction::SetLogin { enabled: false, .. } => {
            return unsupported_action("Netezza no bloquea usuarios a mano: solo los bloquea tras intentos fallidos y los desbloquea con RESET ACCOUNT")
        }
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", ident(D, name)),
        SecurityAction::CreateRole { name } => format!("CREATE GROUP {};", ident(D, grantee(name).1)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP GROUP {};", ident(D, grantee(name).1)),
        SecurityAction::Grant { privileges: p, object, to, grantable } => format!("GRANT {}{} TO {}{};", privileges(p)?, on(object), to_whom(to), option(*grantable)),
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {}{} FROM {};", privileges(p)?, on(object), to_whom(from)),
        SecurityAction::AddMember { role, member } => format!("ALTER GROUP {} ADD USER {};", ident(D, grantee(role).1), ident(D, grantee(member).1)),
        SecurityAction::RemoveMember { role, member } => format!("ALTER GROUP {} DROP USER {};", ident(D, grantee(role).1), ident(D, grantee(member).1)),
    })
}

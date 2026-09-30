//! Db2 for i and Db2 for z/OS. Their users are the operating system's
//! (IBM i user profiles, RACF on z/OS): SQL grants and revokes privileges,
//! it doesn't create users.
//!
//! - **IBM i**: user and group profiles from `QSYS2.USER_INFO` (a group
//!   profile is shown as a role, with its members), privileges on tables
//!   and views from `QSYS2.SYSTABAUTH` and on routines from
//!   `QSYS2.SYSROUTINEAUTH`. There are no SQL roles.
//! - **z/OS**: `SYSIBM.SYSUSERAUTH` (system privileges), `SYSDBAUTH`,
//!   `SYSSCHEMAAUTH`, `SYSTABAUTH` and the roles of `SYSIBM.SYSROLES`
//!   (grantee type `L`), which a user gets through a trusted context, not
//!   a GRANT: they're named `role:<name>` because GRANT says `TO ROLE`.

use super::{db2_auths, first, get, ident, lit, privileges, role, rows, user, Dialect, Row};
use crate::OdbcSession;
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::{HashMap, HashSet};

pub fn spec_i() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "INDEX", "REFERENCES", "EXECUTE", "ALL"],
        object_kinds: vec![kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
        create_user: false,
        create_role: false,
        passwords: false,
        membership: false,
        per_database: false,
    }
}

pub fn spec_z() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "INDEX", "REFERENCES", "TRIGGER", "CREATEIN", "ALTERIN", "DROPIN",
            "BINDADD", "CREATEDBA", "CREATEDBC", "CREATEALIAS", "CREATETMTABLE", "MONITOR1", "MONITOR2", "DISPLAY", "TRACE",
            "SQLADM", "DATAACCESS", "ACCESSCTRL", "SYSOPR", "SYSCTRL", "SYSADM", "ALL",
        ],
        // "" = the subsystem (system privileges).
        object_kinds: vec!["", "schema", kinds::TABLE, kinds::VIEW],
        create_user: false,
        create_role: true,
        passwords: false,
        membership: false,
        per_database: false,
    }
}

// IBM i ----------------------------------------------------------------------

/// `*GRP1 *GRP2`, `*NONE`… as a list.
fn names(v: &str) -> Vec<String> {
    v.split(|c: char| c.is_whitespace() || c == ',')
        .map(str::trim)
        .filter(|x| !x.is_empty() && !x.eq_ignore_ascii_case("*NONE"))
        .map(str::to_string)
        .collect()
}

/// A profile's groups: its group profile and the supplemental ones.
fn groups_of(u: &Row) -> Vec<String> {
    let mut g = names(get(u, "group_profile_name"));
    g.extend(names(get(u, "supplemental_group_list")));
    g
}

pub(super) fn i_principal(u: &Row, is_group: bool) -> Principal {
    let name = get(u, "authorization_name").to_string();
    let special = get(u, "special_authorities");
    let mut details = Vec::new();
    for (k, label) in [
        ("text_description", "Descripción"),
        ("user_class_name", "Clase"),
        ("special_authorities", "Autorizaciones especiales"),
        ("previous_signon", "Último ingreso"),
        ("password_expiration_interval", "Vencimiento de la contraseña"),
    ] {
        let v = get(u, k);
        if !v.is_empty() && v != "*NONE" {
            details.push((label.to_string(), v.to_string()));
        }
    }
    let base = if is_group { role(Dialect::Db2i, &name) } else { user(&name) };
    Principal {
        superuser: Some(special.contains("*ALLOBJ") || special.contains("*SECADM")),
        disabled: Some(get(u, "status").eq_ignore_ascii_case("*DISABLED")),
        can_login: if is_group { Some(false) } else { Some(!get(u, "status").eq_ignore_ascii_case("*DISABLED")) },
        member_of: groups_of(u),
        system: name.starts_with('Q') || name.eq_ignore_ascii_case("PUBLIC"),
        details,
        ..base
    }
}

async fn i_profiles(s: &OdbcSession) -> Result<(Vec<Row>, HashSet<String>)> {
    let all = rows(s, "SELECT * FROM QSYS2.USER_INFO").await?;
    // A group profile: one that's someone's group, or flagged as such.
    let mut groups: HashSet<String> = all.iter().flat_map(groups_of).map(|g| g.to_ascii_uppercase()).collect();
    for u in &all {
        if get(u, "group_member_indicator").eq_ignore_ascii_case("YES") || get(u, "group_id_number").parse::<u64>().is_ok_and(|n| n > 0) {
            groups.insert(get(u, "authorization_name").to_ascii_uppercase());
        }
    }
    Ok((all, groups))
}

pub async fn i_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let (all, groups) = i_profiles(s).await?;
    Ok(all.iter().map(|u| i_principal(u, groups.contains(&get(u, "authorization_name").to_ascii_uppercase()))).collect())
}

fn table_kind(t: &str) -> &'static str {
    if t.eq_ignore_ascii_case("V") {
        kinds::VIEW
    } else {
        kinds::TABLE
    }
}

pub async fn i_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let (all, _) = i_profiles(s).await?;
    let name = super::grantee(principal).1;
    let mut who = vec![(name.to_string(), None::<String>)];
    if let Some(u) = all.iter().find(|u| get(u, "authorization_name").eq_ignore_ascii_case(name)) {
        who.extend(groups_of(u).into_iter().map(|g| (g.clone(), Some(g))));
    }
    let mut out = Vec::new();
    for (n, via) in who {
        let tables = format!(
            "SELECT a.*, t.TABLE_TYPE AS OBJTYPE FROM QSYS2.SYSTABAUTH a LEFT JOIN QSYS2.SYSTABLES t ON t.TABLE_SCHEMA = a.TABLE_SCHEMA AND t.TABLE_NAME = a.TABLE_NAME WHERE a.GRANTEE = {}",
            lit(&n)
        );
        match rows(s, &tables).await {
            Ok(rs) => {
                for r in rs {
                    let object = format!("{}.{}", get(&r, "table_schema"), get(&r, "table_name"));
                    out.extend(db2_auths(&r, Some(object), Some(table_kind(get(&r, "objtype"))), &via));
                }
            }
            Err(e) if via.is_none() => return Err(e),
            Err(_) => {}
        }
        let routines = format!("SELECT * FROM QSYS2.SYSROUTINEAUTH WHERE GRANTEE = {}", lit(&n));
        for r in rows(s, &routines).await.unwrap_or_default() {
            let kind = if get(&r, "routine_type").eq_ignore_ascii_case("FUNCTION") { kinds::FUNCTION } else { kinds::PROCEDURE };
            let schema = get(&r, "specific_schema");
            let object = format!("{}.{}", if schema.is_empty() { get(&r, "routine_schema") } else { schema }, get(&r, "routine_name"));
            let grantable = get(&r, "is_grantable").eq_ignore_ascii_case("YES");
            out.push(Grant { privilege: "EXECUTE".into(), object: Some(object), object_kind: Some(kind.into()), grantable, denied: false, via: via.clone() });
        }
    }
    Ok(out)
}

// z/OS -----------------------------------------------------------------------

/// Grantees of the authorization tables: `(name, is a role)`.
const Z_GRANTEES: &str = "SELECT DISTINCT GRANTEE, GRANTEETYPE FROM SYSIBM.SYSUSERAUTH
 UNION SELECT DISTINCT GRANTEE, GRANTEETYPE FROM SYSIBM.SYSDBAUTH
 UNION SELECT DISTINCT GRANTEE, GRANTEETYPE FROM SYSIBM.SYSSCHEMAAUTH
 UNION SELECT DISTINCT GRANTEE, GRANTEETYPE FROM SYSIBM.SYSTABAUTH WHERE GRANTEETYPE <> 'P'";

pub async fn z_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Db2z;
    let sysadm: HashSet<String> =
        first(s, "SELECT GRANTEE FROM SYSIBM.SYSUSERAUTH WHERE SYSADMAUTH IN ('Y', 'G') OR SYSCTRLAUTH IN ('Y', 'G')").await.unwrap_or_default().into_iter().collect();
    let mut out: Vec<Principal> = Vec::new();
    for r in rows(s, Z_GRANTEES).await? {
        let name = get(&r, "grantee");
        if name.is_empty() || get(&r, "granteetype").eq_ignore_ascii_case("L") {
            continue;
        }
        if !out.iter().any(|p| p.name == name) {
            out.push(Principal {
                can_login: None,
                superuser: Some(sysadm.contains(name)),
                system: name.eq_ignore_ascii_case("PUBLIC") || name.starts_with("SYSIBM"),
                details: vec![("Autenticación".into(), "del sistema (RACF u otro gestor de seguridad)".into())],
                ..user(name)
            });
        }
    }
    for r in rows(s, "SELECT NAME, DEFINER, REMARKS FROM SYSIBM.SYSROLES").await.unwrap_or_default() {
        let mut p = role(d, get(&r, "name"));
        p.details.push(("Miembros".into(), "los usuarios de un contexto de confianza (CREATE TRUSTED CONTEXT … DEFAULT ROLE)".into()));
        if !get(&r, "definer").is_empty() {
            p.details.push(("Definido por".into(), get(&r, "definer").to_string()));
        }
        out.push(p);
    }
    Ok(out)
}

/// The z/OS catalog's names of some privileges.
pub(super) fn z_privilege(g: Grant) -> Grant {
    let privilege = match g.privilege.as_str() {
        "MON1" => "MONITOR1".to_string(),
        "MON2" => "MONITOR2".to_string(),
        "CREATETMTAB" => "CREATETMTABLE".to_string(),
        "REFERENCES" | "REFERENCE" => "REFERENCES".to_string(),
        p => p.to_string(),
    };
    Grant { privilege, ..g }
}

pub async fn z_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let (is_role, name) = super::grantee(principal);
    let who = format!("GRANTEE = {} AND GRANTEETYPE = '{}'", lit(name), if is_role { "L" } else { " " });
    let mut out = Vec::new();
    for r in rows(s, &format!("SELECT * FROM SYSIBM.SYSUSERAUTH WHERE {who}")).await? {
        out.extend(db2_auths(&r, None, None, &None));
    }
    for r in rows(s, &format!("SELECT * FROM SYSIBM.SYSDBAUTH WHERE {who}")).await.unwrap_or_default() {
        out.extend(db2_auths(&r, Some(get(&r, "name").to_string()), Some("database"), &None));
    }
    for r in rows(s, &format!("SELECT * FROM SYSIBM.SYSSCHEMAAUTH WHERE {who}")).await.unwrap_or_default() {
        out.extend(db2_auths(&r, Some(get(&r, "schemaname").to_string()), Some("schema"), &None));
    }
    let tables = format!(
        "SELECT a.*, t.TYPE AS OBJTYPE FROM SYSIBM.SYSTABAUTH a LEFT JOIN SYSIBM.SYSTABLES t ON t.CREATOR = a.TCREATOR AND t.NAME = a.TTNAME WHERE a.{who}"
    );
    for r in rows(s, &tables).await.unwrap_or_default() {
        let object = format!("{}.{}", get(&r, "tcreator"), get(&r, "ttname"));
        out.extend(db2_auths(&r, Some(object), Some(table_kind(get(&r, "objtype"))), &None));
    }
    // The same privilege from several grantors: once, grantable if any is.
    let mut merged: HashMap<(String, Option<String>), Grant> = HashMap::new();
    let mut order = Vec::new();
    for g in out.into_iter().map(z_privilege) {
        let k = (g.privilege.clone(), g.object.clone());
        match merged.get_mut(&k) {
            Some(x) => x.grantable |= g.grantable,
            None => {
                order.push(k.clone());
                merged.insert(k, g);
            }
        }
    }
    Ok(order.into_iter().filter_map(|k| merged.remove(&k)).collect())
}

// scripts --------------------------------------------------------------------

fn on(d: Dialect, o: &ObjectRef) -> String {
    let q = super::qualified(d, o);
    match o.kind.as_str() {
        "schema" => format!(" ON SCHEMA {}", ident(d, &o.name)),
        k if k == kinds::PROCEDURE => format!(" ON PROCEDURE {q}"),
        k if k == kinds::FUNCTION => format!(" ON FUNCTION {q}"),
        _ => format!(" ON TABLE {q}"),
    }
}

fn to_whom(d: Dialect, name: &str) -> String {
    match super::grantee(name) {
        (true, r) => format!("ROLE {}", ident(d, r)),
        (false, n) => ident(d, n),
    }
}

pub fn script(d: Dialect, a: &SecurityAction) -> Result<String> {
    let z = d == Dialect::Db2z;
    Ok(match a {
        SecurityAction::CreateUser { .. }
        | SecurityAction::SetPassword { .. }
        | SecurityAction::SetLogin { .. }
        | SecurityAction::Drop { kind: PrincipalKind::User, .. } => {
            return Err(Error::Unsupported(if z {
                "los usuarios de Db2 for z/OS son del gestor de seguridad del sistema (RACF): no se crean, borran ni bloquean desde SQL".into()
            } else {
                "los usuarios de IBM i son perfiles del sistema operativo (CRTUSRPRF, CHGUSRPRF): no se crean, borran ni bloquean desde SQL".into()
            }))
        }
        SecurityAction::CreateRole { name } if z => format!("CREATE ROLE {};", ident(d, super::grantee(name).1)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } if z => format!("DROP ROLE {};", ident(d, super::grantee(name).1)),
        SecurityAction::CreateRole { .. } | SecurityAction::Drop { .. } => {
            return Err(Error::Unsupported(
                "IBM i no tiene roles de SQL: los grupos son perfiles de grupo del sistema operativo (CRTUSRPRF … GRPPRF)".into(),
            ))
        }
        SecurityAction::AddMember { .. } | SecurityAction::RemoveMember { .. } => {
            return Err(Error::Unsupported(if z {
                "Db2 for z/OS no otorga roles con GRANT: un usuario toma un rol a través de un contexto de confianza (CREATE TRUSTED CONTEXT … DEFAULT ROLE)".into()
            } else {
                "en IBM i los miembros de un grupo se definen en el perfil del usuario (CHGUSRPRF … GRPPRF / SUPGRPPRF), no desde SQL".into()
            }))
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            let option = if *grantable { " WITH GRANT OPTION" } else { "" };
            match object {
                Some(o) => format!("GRANT {}{} TO {}{option};", privileges(p)?, on(d, o), to_whom(d, to)),
                None if z => format!("GRANT {} TO {}{option};", privileges(p)?, to_whom(d, to)),
                None => return Err(Error::Query("elegí una tabla, una vista o una rutina: IBM i no tiene permisos de SQL sobre todo el sistema".into())),
            }
        }
        SecurityAction::Revoke { privileges: p, object, from } => match object {
            Some(o) => format!("REVOKE {}{} FROM {};", privileges(p)?, on(d, o), to_whom(d, from)),
            None if z => format!("REVOKE {} FROM {};", privileges(p)?, to_whom(d, from)),
            None => return Err(Error::Query("elegí una tabla, una vista o una rutina".into())),
        },
    })
}


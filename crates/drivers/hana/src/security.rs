//! Users, roles and permissions (docs/users-and-permissions.md) for SAP HANA.
//!
//! Read from the SYS catalog: USERS, ROLES, GRANTED_ROLES and
//! GRANTED_PRIVILEGES (which lists only the direct grants of each grantee;
//! the ones held through roles are followed recursively). Without CATALOG
//! READ a user sees only its own grants and the ones it gave.
//!
//! Repository roles (`package::role`, created by _SYS_REPO) aren't granted
//! with `GRANT`: they go through `_SYS_REPO.GRANT_ACTIVATED_ROLE`.

use crate::{quote as q, text, HanaSession};
use dbine_driver::{
    Error, Grant, ObjectRef, Principal, PrincipalKind, Privilege, Result, SecurityAction,
    SecuritySpec,
};
use hdbconnect_async::HdbValue;
use std::collections::{HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            // On schemas and objects.
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "EXECUTE",
            "ALTER",
            "DROP",
            "INDEX",
            "TRIGGER",
            "REFERENCES",
            "CREATE ANY",
            "DEBUG",
            "ALL PRIVILEGES",
            // System privileges (granted without an object).
            "CATALOG READ",
            "CREATE SCHEMA",
            "USER ADMIN",
            "ROLE ADMIN",
            "DATA ADMIN",
            "BACKUP ADMIN",
            "MONITOR ADMIN",
            "INIFILE ADMIN",
            "TRACE ADMIN",
            "AUDIT ADMIN",
            "SESSION ADMIN",
            "EXPORT",
            "IMPORT",
        ],
        // The objects of the explorer have no schema: they're in the
        // connection's schema, which names them.
        object_kinds: vec![
            "",
            "schema",
            "table",
            "view",
            "procedure",
            "function",
            "sequence",
        ],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

// -- reading -----------------------------------------------------------------

const USERS: &str = "SELECT USER_NAME, USER_DEACTIVATED, CREATOR, TO_VARCHAR(CREATE_TIME, 'YYYY-MM-DD HH24:MI'),
       TO_VARCHAR(VALID_UNTIL, 'YYYY-MM-DD HH24:MI'), TO_VARCHAR(LAST_SUCCESSFUL_CONNECT, 'YYYY-MM-DD HH24:MI'),
       IS_PASSWORD_ENABLED, IS_RESTRICTED, USERGROUP_NAME
  FROM SYS.USERS ORDER BY USER_NAME";
/// Before 2.0 SPS03 (no user groups) and SPS09 (no restricted users).
const USERS_OLD: &str = "SELECT USER_NAME, USER_DEACTIVATED, CREATOR, TO_VARCHAR(CREATE_TIME, 'YYYY-MM-DD HH24:MI'),
       TO_VARCHAR(VALID_UNTIL, 'YYYY-MM-DD HH24:MI'), TO_VARCHAR(LAST_SUCCESSFUL_CONNECT, 'YYYY-MM-DD HH24:MI'),
       CAST(NULL AS NVARCHAR(5)), CAST(NULL AS NVARCHAR(5)), CAST(NULL AS NVARCHAR(5))
  FROM SYS.USERS ORDER BY USER_NAME";
const ROLES: &str =
    "SELECT ROLE_NAME, ROLE_SCHEMA_NAME, CREATOR, TO_VARCHAR(CREATE_TIME, 'YYYY-MM-DD HH24:MI')
  FROM SYS.ROLES ORDER BY ROLE_NAME";
const MEMBERS: &str = "SELECT GRANTEE, ROLE_NAME FROM SYS.GRANTED_ROLES";
const GRANTS: &str =
    "SELECT PRIVILEGE, OBJECT_TYPE, SCHEMA_NAME, OBJECT_NAME, COLUMN_NAME, IS_GRANTABLE
  FROM SYS.GRANTED_PRIVILEGES WHERE GRANTEE = ?";
const ROLES_OF: &str = "SELECT ROLE_NAME FROM SYS.GRANTED_ROLES WHERE GRANTEE = ?";

fn at(r: &[HdbValue<'static>], i: usize) -> Option<String> {
    r.get(i).and_then(text).filter(|s| !s.is_empty())
}

fn yes(v: Option<String>) -> bool {
    v.is_some_and(|v| v.eq_ignore_ascii_case("TRUE"))
}

/// Built-in users: SYS, SYSTEM, the _SYS_* ones and whatever SYS created.
fn system_user(name: &str, creator: Option<&str>) -> bool {
    name == "SYS" || name == "SYSTEM" || name.starts_with("_SYS") || creator == Some("SYS")
}

pub async fn principals(s: &HanaSession) -> Result<Vec<Principal>> {
    let users = match s.rows(USERS, &[]).await {
        Ok(r) => r,
        Err(_) => s.rows(USERS_OLD, &[]).await?,
    };
    let roles = s.rows(ROLES, &[]).await?;
    let mut out = Vec::new();
    for r in &users {
        let name = at(r, 0).unwrap_or_default();
        let creator = at(r, 2);
        let mut details = Vec::new();
        for (label, i) in [("Creado", 3), ("Vence", 4), ("Último ingreso", 5)] {
            if let Some(v) = at(r, i) {
                details.push((label.to_string(), v));
            }
        }
        if let Some(c) = &creator {
            details.push(("Creado por".into(), c.clone()));
        }
        if let Some(v) = at(r, 6) {
            details.push((
                "Contraseña".into(),
                if yes(Some(v)) {
                    "habilitada".into()
                } else {
                    "deshabilitada".into()
                },
            ));
        }
        if yes(at(r, 7)) {
            details.push(("Restringido".into(), "sí".into()));
        }
        if let Some(g) = at(r, 8) {
            details.push(("Grupo de usuarios".into(), g));
        }
        out.push(Principal {
            kind: PrincipalKind::User,
            can_login: Some(true),
            superuser: Some(name == "SYSTEM"),
            disabled: Some(yes(at(r, 1))),
            member_of: Vec::new(),
            details,
            system: system_user(&name, creator.as_deref()),
            name,
        });
    }
    for r in &roles {
        let name = at(r, 0).unwrap_or_default();
        let creator = at(r, 2);
        let mut details = Vec::new();
        if let Some(sc) = at(r, 1) {
            details.push(("Esquema".into(), sc));
        }
        if let Some(c) = &creator {
            details.push(("Creado por".into(), c.clone()));
        }
        if let Some(t) = at(r, 3) {
            details.push(("Creado".into(), t));
        }
        if name.contains("::") {
            details.push(("Tipo".into(), "rol de repositorio".into()));
        }
        out.push(Principal {
            kind: PrincipalKind::Role,
            // Repository roles can't be dropped with DROP ROLE.
            system: name == "PUBLIC" || matches!(creator.as_deref(), Some("SYS" | "_SYS_REPO")),
            details,
            name,
            ..Default::default()
        });
    }
    for m in s.rows(MEMBERS, &[]).await.unwrap_or_default() {
        let (Some(member), Some(role)) = (at(&m, 0), at(&m, 1)) else {
            continue;
        };
        if let Some(p) = out.iter_mut().find(|p| p.name == member) {
            if !p.member_of.contains(&role) {
                p.member_of.push(role);
            }
        }
    }
    Ok(out)
}

/// One row of GRANTED_PRIVILEGES as a grant.
fn grant_of(
    privilege: String,
    ty: &str,
    schema: Option<String>,
    object: Option<String>,
    column: Option<String>,
    grantable: bool,
) -> Grant {
    let (object, object_kind) = match ty {
        "SYSTEMPRIVILEGE" | "" => (None, None),
        "SCHEMA" => (schema.or(object), Some("schema".to_string())),
        _ => {
            let kind = match ty {
                "TABLE" | "VIEW" | "PROCEDURE" | "FUNCTION" | "SEQUENCE" => ty.to_ascii_lowercase(),
                // REMOTESOURCE, APPLICATIONPRIVILEGE, ANALYTICALPRIVILEGE, USERGROUP…
                other => other.to_ascii_lowercase().replace(' ', "_"),
            };
            let name = match (schema, object) {
                (Some(s), Some(o)) => Some(format!("{s}.{o}")),
                (s, o) => o.or(s),
            };
            (name, Some(kind))
        }
    };
    let privilege = match column {
        Some(c) => format!("{privilege} ({c})"),
        None => privilege,
    };
    Grant {
        privilege,
        object,
        object_kind,
        grantable,
        denied: false,
        via: None,
    }
}

pub async fn grants(s: &HanaSession, principal: &str) -> Result<Vec<Grant>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> =
        VecDeque::from([(principal.to_string(), None)]);
    while let Some((name, via)) = queue.pop_front() {
        if !seen.insert(name.clone()) || seen.len() > 256 {
            continue;
        }
        for r in s.rows(GRANTS, &[&name]).await? {
            let ty = at(&r, 1).unwrap_or_default();
            let mut g = grant_of(
                at(&r, 0).unwrap_or_default(),
                &ty,
                at(&r, 2),
                at(&r, 3),
                at(&r, 4),
                yes(at(&r, 5)),
            );
            g.via = via.clone();
            out.push(g);
        }
        for r in s.rows(ROLES_OF, &[&name]).await? {
            if let Some(role) = at(&r, 0) {
                let v = via.clone().unwrap_or_else(|| role.clone());
                queue.push_back((role, Some(v)));
            }
        }
    }
    Ok(out)
}

// -- what can be granted ---------------------------------------------------

/// The privileges the server knows and their type: SYSTEMPRIVILEGE,
/// OBJECTPRIVILEGE, PACKAGEPRIVILEGE (repository packages) or
/// APPLICATIONPRIVILEGE (XS applications).
const PRIVILEGES: &str = "SELECT NAME, TYPE FROM SYS.PRIVILEGES";
/// What each object privilege can be granted on.
const OBJECT_PRIVILEGES: &str = "SELECT OBJECT_TYPE, PRIVILEGE FROM SYS.OBJECT_PRIVILEGES";

const SCHEMA_TABLE_VIEW: &[&str] = &["schema", "table", "view"];
const SCHEMA_ROUTINES: &[&str] = &["schema", "procedure", "function"];

/// Object (and schema) privileges per kind, in the order the tab offers
/// them: "Object Privileges" and `<schema_privilege>` of GRANT in the SAP
/// HANA 2.0 SPS08 SQL Reference
/// (https://help.sap.com/docs/SAP_HANA_PLATFORM/4fe29514fd584807ac9f2a04f6754767/20f674e1751910148a8b990d33efbdc5.html).
const OBJECT: &[(&str, &[&str])] = &[
    ("SELECT", &["schema", "table", "view", "sequence"]),
    ("INSERT", SCHEMA_TABLE_VIEW),
    ("UPDATE", SCHEMA_TABLE_VIEW),
    (
        "DELETE",
        &["schema", "table", "view", "procedure", "function"],
    ),
    ("EXECUTE", SCHEMA_ROUTINES),
    (
        "ALTER",
        &["schema", "table", "view", "procedure", "function"],
    ),
    (
        "DROP",
        &[
            "schema",
            "table",
            "view",
            "sequence",
            "procedure",
            "function",
        ],
    ),
    ("INDEX", &["schema", "table"]),
    ("TRIGGER", &["schema", "table"]),
    ("REFERENCES", &["schema", "table"]),
    (
        "CREATE ANY",
        &[
            "schema",
            "table",
            "view",
            "sequence",
            "procedure",
            "function",
        ],
    ),
    // DEBUG on views: calculation views.
    ("DEBUG", &["schema", "view", "procedure", "function"]),
    ("DEBUG MODIFY", SCHEMA_ROUTINES),
    ("ALL PRIVILEGES", SCHEMA_TABLE_VIEW),
    ("UNMASKED", SCHEMA_TABLE_VIEW),
    ("SELECT METADATA", &["schema", "table"]),
    ("SELECT CDS METADATA", &["schema", "table"]),
    ("CREATE TEMPORARY TABLE", &["schema"]),
    ("CREATE VIRTUAL PACKAGE", &["schema"]),
    ("CREATE OBJECT STRUCTURED PRIVILEGE", &["schema", "view"]),
    ("CLIENTSIDE ENCRYPTION COLUMN KEY ADMIN", &["schema"]),
    ("SQLSCRIPT LOGGING", SCHEMA_ROUTINES),
    // On remote sources, users, user groups, agents and column keys, which
    // the tab doesn't offer.
    ("CREATE VIRTUAL TABLE", &[]),
    ("CREATE VIRTUAL FUNCTION", &[]),
    ("CREATE VIRTUAL PROCEDURE", &[]),
    ("CREATE REMOTE SUBSCRIPTION", &[]),
    ("PROCESS REMOTE SUBSCRIPTION EXCEPTION", &[]),
    ("LINKED DATABASE", &[]),
    ("REMOTE EXECUTE", &[]),
    ("REMOTE TABLE ADMIN", &[]),
    ("ATTACH DEBUGGER", &[]),
    ("AGENT MESSAGING", &[]),
    ("USERGROUP OPERATOR", &[]),
    ("USAGE", &[]),
];

/// System privileges (granted without an object), for when SYS.PRIVILEGES
/// can't be read: `<system_privilege>` of the same GRANT page, and the
/// repository's (XS classic) from the Security Guide. Some can only be
/// granted in the system database.
const SYSTEM: &[&str] = &[
    "ADAPTER ADMIN",
    "AGENT ADMIN",
    "ALTER CLIENTSIDE ENCRYPTION KEYPAIR",
    "AUDIT ADMIN",
    "AUDIT OPERATOR",
    "AUDIT READ",
    "BACKUP ADMIN",
    "BACKUP OPERATOR",
    "CATALOG READ",
    "CERTIFICATE ADMIN",
    "CLIENT PARAMETER ADMIN",
    "CREATE CLIENTSIDE ENCRYPTION KEYPAIR",
    "CREATE REMOTE SOURCE",
    "CREATE SCENARIO",
    "CREATE SCHEMA",
    "CREATE STRUCTURED PRIVILEGE",
    "CREDENTIAL ADMIN",
    "DATA ADMIN",
    "DATABASE ADMIN",
    "DATABASE AUDIT ADMIN",
    "DATABASE BACKUP ADMIN",
    "DATABASE BACKUP OPERATOR",
    "DATABASE RECOVERY OPERATOR",
    "DATABASE START",
    "DATABASE STOP",
    "DROP CLIENTSIDE ENCRYPTION KEYPAIR",
    "ENCRYPTION ROOT KEY ADMIN",
    "EXPORT",
    "EXTENDED STORAGE ADMIN",
    "IMPORT",
    "INIFILE ADMIN",
    "LDAP ADMIN",
    "LICENSE ADMIN",
    "LOG ADMIN",
    "MONITOR ADMIN",
    "OPTIMIZER ADMIN",
    "PARTITION ADMIN",
    "RESOURCE ADMIN",
    "ROLE ADMIN",
    "SAVEPOINT ADMIN",
    "SCENARIO ADMIN",
    "SERVICE ADMIN",
    "SESSION ADMIN",
    "SSL ADMIN",
    "STRUCTUREDPRIVILEGE ADMIN",
    "SYSTEM REPLICATION ADMIN",
    "TABLE ADMIN",
    "TRACE ADMIN",
    "TRUST ADMIN",
    "USER ADMIN",
    "VERSION ADMIN",
    "WORKLOAD ADMIN",
    "WORKLOAD ANALYZE ADMIN",
    "WORKLOAD CAPTURE ADMIN",
    "WORKLOAD REPLAY ADMIN",
    "REPO.EXPORT",
    "REPO.IMPORT",
    "REPO.MAINTAIN_DELIVERY_UNITS",
    "REPO.WORK_IN_FOREIGN_WORKSPACE",
    "REPO.CONFIGURE",
    "REPO.MODIFY_CHANGE",
    "REPO.MODIFY_OWN_CONTRIBUTION",
    "REPO.MODIFY_FOREIGN_CONTRIBUTION",
];

/// The privileges this server can grant: SYS.PRIVILEGES, with
/// SYS.OBJECT_PRIVILEGES for what each object privilege applies to, so they
/// follow its version (HANA Cloud has its own); the documented ones if
/// SYS.PRIVILEGES can't be read.
pub async fn available(s: &HanaSession) -> Result<Vec<Privilege>> {
    let pairs = |rows: Vec<Vec<HdbValue<'static>>>| -> Vec<(String, String)> {
        rows.iter()
            .filter_map(|r| Some((at(r, 0)?, at(r, 1)?)))
            .collect()
    };
    let privileges = s.rows(PRIVILEGES, &[]).await.map(pairs).unwrap_or_default();
    if privileges.is_empty() {
        return Ok(static_privileges());
    }
    let object = s
        .rows(OBJECT_PRIVILEGES, &[])
        .await
        .map(pairs)
        .unwrap_or_default();
    Ok(listed(&privileges, &object))
}

/// The spec's kinds an OBJECT_TYPE of SYS.OBJECT_PRIVILEGES stands for:
/// none for remote sources, synonyms, graph workspaces, users…
fn kinds_of(object_type: &str) -> Vec<&'static str> {
    let t = object_type.trim().to_ascii_uppercase();
    let mut out = Vec::new();
    if t.contains("SCHEMA") {
        out.push("schema");
    }
    // TABLE, ROW TABLE, COLUMN TABLE, VIRTUAL TABLE…
    if t.ends_with("TABLE") {
        out.push("table");
    }
    // VIEW, CALCULATION VIEW…
    if t.ends_with("VIEW") {
        out.push("view");
    }
    if t.contains("SEQUENCE") {
        out.push("sequence");
    }
    if t.contains("PROCEDURE") {
        out.push("procedure");
    }
    if t.contains("FUNCTION") {
        out.push("function");
    }
    out
}

/// `privileges` (name, type) and `object` (object type, privilege) as
/// privileges: system ones on "", object ones on the kinds `object` names
/// (the documented kinds for one it doesn't describe, or for a kind it
/// never mentions; every object kind for one this crate doesn't know).
/// Package and application privileges are left out.
fn listed(privileges: &[(String, String)], object: &[(String, String)]) -> Vec<Privilege> {
    let all: Vec<&str> = spec()
        .object_kinds
        .into_iter()
        .filter(|k| !k.is_empty())
        .collect();
    let covered: HashSet<&str> = object.iter().flat_map(|(t, _)| kinds_of(t)).collect();
    let mut out: Vec<Privilege> = Vec::new();
    for (name, ty) in privileges {
        if !valid(name) {
            continue;
        }
        let on: Vec<&str> = match ty.as_str() {
            "SYSTEMPRIVILEGE" => vec![""],
            "OBJECTPRIVILEGE" => {
                let documented = OBJECT.iter().find(|(n, _)| n == name).map(|(_, on)| *on);
                let types: Vec<&str> = object
                    .iter()
                    .filter(|(_, p)| p == name)
                    .map(|(t, _)| t.as_str())
                    .collect();
                if types.is_empty() {
                    documented.map_or(all.clone(), <[&str]>::to_vec)
                } else {
                    let uncovered = documented
                        .unwrap_or_default()
                        .iter()
                        .copied()
                        .filter(|k| !covered.contains(k));
                    types
                        .into_iter()
                        .flat_map(kinds_of)
                        .chain(uncovered)
                        .collect()
                }
            }
            _ => continue,
        };
        // A name of two types (system and object) is one privilege.
        let p = match out.iter_mut().find(|p| &p.name == name) {
            Some(p) => p,
            None => {
                out.push(Privilege::new(name.clone(), &[]));
                out.last_mut().unwrap()
            }
        };
        for k in on {
            if !p.on.iter().any(|o| o == k) {
                p.on.push(k.to_string());
            }
        }
    }
    out.retain(|p| !p.on.is_empty());
    let kinds = spec().object_kinds;
    for p in &mut out {
        p.on.sort_by_key(|k| kinds.iter().position(|x| x == k));
    }
    let rank = |n: &str| {
        OBJECT
            .iter()
            .position(|(o, _)| *o == n)
            .unwrap_or(OBJECT.len())
    };
    out.sort_by(|a, b| (rank(&a.name), &a.name).cmp(&(rank(&b.name), &b.name)));
    out
}

fn static_privileges() -> Vec<Privilege> {
    OBJECT
        .iter()
        .filter(|(_, on)| !on.is_empty())
        .map(|(name, on)| Privilege::new(*name, on))
        .chain(SYSTEM.iter().map(|n| Privilege::new(*n, &[""])))
        .collect()
}

// -- scripts -----------------------------------------------------------------

/// A new user's or role's name: a plain identifier is uppercased, as HANA
/// does with unquoted names; anything else is kept as typed.
fn new_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Query("escribí el nombre".into()));
    }
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    Ok(q(&if plain {
        name.to_ascii_uppercase()
    } else {
        name.to_string()
    }))
}

/// A password as HANA takes it: a quoted identifier, so it can't have `"`.
fn password(p: &str) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("escribí la contraseña del usuario".into()));
    }
    if p.contains('"') {
        return Err(Error::Query(
            "en SAP HANA la contraseña no puede tener comillas dobles (\")".into(),
        ));
    }
    Ok(format!("\"{p}\""))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A repository role (`package::role`): granted through _SYS_REPO.
fn repo_role(role: &str) -> bool {
    role.contains("::")
}

fn on(o: &ObjectRef) -> String {
    match o.kind.as_str() {
        "schema" | "database" => format!("SCHEMA {}", q(&o.name)),
        _ => match o.schema() {
            Some(sc) => format!("{}.{}", q(sc), q(&o.name)),
            None => q(&o.name),
        },
    }
}

/// A privilege's name: words of letters, digits and underscores (not
/// starting with a digit), joined by spaces or, in a component's privilege,
/// by dots (`REPO.EXPORT`, `CREATE X509 PROVIDER`).
fn valid(name: &str) -> bool {
    let word = |w: &str| {
        w.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    name.split_whitespace().next().is_some()
        && name.split_whitespace().all(|w| w.split('.').all(word))
}

/// Privilege names (see [`valid`]).
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out: Vec<String> = Vec::new();
    for x in p {
        let name = x.trim();
        if !valid(name) {
            return Err(Error::Query(format!("«{x}» no es un permiso de SAP HANA")));
        }
        let name = name
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase();
        if !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(out.join(", "))
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => format!(
            "CREATE USER {} PASSWORD {};\n\
             -- SAP HANA le pide cambiar la contraseña en el primer ingreso; para evitarlo, agregá NO FORCE_FIRST_PASSWORD_CHANGE.",
            new_name(name)?,
            password(pw.as_deref().unwrap_or_default())?
        ),
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", new_name(name)?),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!(
            "DROP USER {n};\n-- Si el usuario tiene objetos en su esquema, hace falta CASCADE, que los borra junto con él:\n-- DROP USER {c} CASCADE;",
            n = q(name),
            c = comment_text(&q(name))
        ),
        SecurityAction::SetPassword { name, password: pw } => format!("ALTER USER {} PASSWORD {};", q(name), password(pw)?),
        SecurityAction::SetLogin { name, enabled } => {
            format!("ALTER USER {} {} USER NOW;", q(name), if *enabled { "ACTIVATE" } else { "DEACTIVATE" })
        }
        SecurityAction::Grant { privileges: p, object: None, to, grantable } => {
            format!("GRANT {} TO {}{};", privileges(p)?, q(to), if *grantable { " WITH ADMIN OPTION" } else { "" })
        }
        SecurityAction::Grant { privileges: p, object: Some(o), to, grantable } => {
            format!("GRANT {} ON {} TO {}{};", privileges(p)?, on(o), q(to), if *grantable { " WITH GRANT OPTION" } else { "" })
        }
        SecurityAction::Revoke { privileges: p, object: None, from } => format!("REVOKE {} FROM {};", privileges(p)?, q(from)),
        SecurityAction::Revoke { privileges: p, object: Some(o), from } => {
            format!("REVOKE {} ON {} FROM {};", privileges(p)?, on(o), q(from))
        }
        SecurityAction::AddMember { role, member } if repo_role(role) => {
            format!("CALL \"_SYS_REPO\".\"GRANT_ACTIVATED_ROLE\"({}, {});", lit(role), lit(member))
        }
        SecurityAction::RemoveMember { role, member } if repo_role(role) => {
            format!("CALL \"_SYS_REPO\".\"REVOKE_ACTIVATED_ROLE\"({}, {});", lit(role), lit(member))
        }
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", q(role), q(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", q(role), q(member)),
    })
}

/// Text for a `--` comment line: a server-controlled name can't end the
/// comment and turn the rest of the line into a statement. Line breaks
/// (CR, LF, NEL, U+2028/U+2029) and other control characters become `?`.
fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                '?'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef {
            kind: kind.into(),
            schema: schema.map(Into::into),
            name: name.into(),
        })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert!(s(SecurityAction::CreateUser {
            name: "ana".into(),
            password: Some("p'w\\1".into())
        })
        .starts_with("CREATE USER \"ANA\" PASSWORD \"p'w\\1\";\n-- "));
        assert!(s(SecurityAction::CreateUser {
            name: "Ana López".into(),
            password: Some("x".into())
        })
        .starts_with("CREATE USER \"Ana López\" PASSWORD"));
        assert!(script(&SecurityAction::CreateUser {
            name: "ana".into(),
            password: Some("a\"b".into())
        })
        .is_err());
        assert!(script(&SecurityAction::CreateUser {
            name: "ana".into(),
            password: None
        })
        .is_err());
        assert!(script(&SecurityAction::CreateRole { name: "  ".into() }).is_err());
        assert_eq!(
            s(SecurityAction::CreateRole {
                name: "lectores".into()
            }),
            "CREATE ROLE \"LECTORES\";"
        );
        assert_eq!(
            s(SecurityAction::Drop {
                name: "LECT\"X".into(),
                kind: PrincipalKind::Role
            }),
            "DROP ROLE \"LECT\"\"X\";"
        );
        assert!(s(SecurityAction::Drop {
            name: "ANA".into(),
            kind: PrincipalKind::User
        })
        .starts_with("DROP USER \"ANA\";\n-- "));
        assert_eq!(
            s(SecurityAction::SetPassword {
                name: "ANA".into(),
                password: "n'1".into()
            }),
            "ALTER USER \"ANA\" PASSWORD \"n'1\";"
        );
        assert_eq!(
            s(SecurityAction::SetLogin {
                name: "ANA".into(),
                enabled: false
            }),
            "ALTER USER \"ANA\" DEACTIVATE USER NOW;"
        );
        assert_eq!(
            s(SecurityAction::SetLogin {
                name: "ANA".into(),
                enabled: true
            }),
            "ALTER USER \"ANA\" ACTIVATE USER NOW;"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["catalog  read".into()],
                object: None,
                to: "ANA".into(),
                grantable: true
            }),
            "GRANT CATALOG READ TO \"ANA\" WITH ADMIN OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["select".into(), "UPDATE".into()],
                object: obj("table", Some("VENTAS"), "FAC\"T"),
                to: "ANA".into(),
                grantable: true
            }),
            "GRANT SELECT, UPDATE ON \"VENTAS\".\"FAC\"\"T\" TO \"ANA\" WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["SELECT".into()],
                object: obj("table", None, "T"),
                to: "R".into(),
                grantable: false
            }),
            "GRANT SELECT ON \"T\" TO \"R\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke {
                privileges: vec!["INSERT".into()],
                object: obj("schema", None, "VENTAS"),
                from: "R".into()
            }),
            "REVOKE INSERT ON SCHEMA \"VENTAS\" FROM \"R\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke {
                privileges: vec!["USER ADMIN".into()],
                object: None,
                from: "R".into()
            }),
            "REVOKE USER ADMIN FROM \"R\";"
        );
        assert_eq!(
            s(SecurityAction::AddMember {
                role: "LECT".into(),
                member: "ANA".into()
            }),
            "GRANT \"LECT\" TO \"ANA\";"
        );
        assert_eq!(
            s(SecurityAction::RemoveMember {
                role: "LECT".into(),
                member: "ANA".into()
            }),
            "REVOKE \"LECT\" FROM \"ANA\";"
        );
        assert_eq!(
            s(SecurityAction::AddMember {
                role: "sap.hana::O'Admin".into(),
                member: "ANA".into()
            }),
            "CALL \"_SYS_REPO\".\"GRANT_ACTIVATED_ROLE\"('sap.hana::O''Admin', 'ANA');"
        );
        assert_eq!(
            s(SecurityAction::RemoveMember {
                role: "sap.hana::Admin".into(),
                member: "ANA".into()
            }),
            "CALL \"_SYS_REPO\".\"REVOKE_ACTIVATED_ROLE\"('sap.hana::Admin', 'ANA');"
        );
        assert!(script(&SecurityAction::Grant {
            privileges: vec!["SELECT; DROP TABLE x".into()],
            object: None,
            to: "a".into(),
            grantable: false
        })
        .is_err());
        assert!(script(&SecurityAction::Grant {
            privileges: vec![],
            object: None,
            to: "a".into(),
            grantable: false
        })
        .is_err());
    }

    /// Every privilege, on each of its kinds, makes a GRANT script.
    fn grantable(list: &[Privilege]) {
        let kinds = spec().object_kinds;
        for p in list {
            assert!(!p.on.is_empty(), "{}", p.name);
            for k in &p.on {
                assert!(kinds.contains(&k.as_str()), "{} on {k}", p.name);
                let object = (!k.is_empty()).then(|| ObjectRef {
                    kind: k.clone(),
                    schema: (k != "schema").then(|| "VENTAS".into()),
                    name: "OBJ".into(),
                });
                let s = script(&SecurityAction::Grant {
                    privileges: vec![p.name.clone()],
                    object,
                    to: "ANA".into(),
                    grantable: false,
                })
                .unwrap_or_else(|e| panic!("{} on {k}: {e}", p.name));
                assert!(s.starts_with(&format!("GRANT {} ", p.name)), "{s}");
            }
        }
    }

    fn on<'a>(list: &'a [Privilege], n: &str) -> Option<Vec<&'a str>> {
        list.iter()
            .find(|p| p.name == n)
            .map(|p| p.on.iter().map(String::as_str).collect())
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn static_privileges_make_scripts() {
        let list = static_privileges();
        grantable(&list);
        assert_eq!(on(&list, "REPO.EXPORT"), Some(vec![""]));
        assert_eq!(
            on(&list, "EXECUTE"),
            Some(vec!["schema", "procedure", "function"])
        );
        assert_eq!(on(&list, "CREATE VIRTUAL TABLE"), None);
    }

    #[test]
    fn maps_the_catalog() {
        let privileges = pairs(&[
            ("CATALOG READ", "SYSTEMPRIVILEGE"),
            ("REPO.EXPORT", "SYSTEMPRIVILEGE"),
            ("CREATE X509 PROVIDER", "SYSTEMPRIVILEGE"),
            ("SELECT", "OBJECTPRIVILEGE"),
            ("DEBUG", "OBJECTPRIVILEGE"),
            ("CREATE VIRTUAL TABLE", "OBJECTPRIVILEGE"),
            ("REMOTE THING", "OBJECTPRIVILEGE"),
            ("NEW THING", "OBJECTPRIVILEGE"),
            ("AGENT MESSAGING", "SYSTEMPRIVILEGE"),
            ("AGENT MESSAGING", "OBJECTPRIVILEGE"),
            ("REPO.READ", "PACKAGEPRIVILEGE"),
            ("BAD;NAME", "SYSTEMPRIVILEGE"),
        ]);
        let object = pairs(&[
            ("TABLE", "SELECT"),
            ("VIEW", "SELECT"),
            ("SEQUENCE", "SELECT"),
            ("SCHEMA", "SELECT"),
            ("CALCULATION VIEW", "DEBUG"),
            ("PROCEDURE", "DEBUG"),
            ("FUNCTION", "DEBUG"),
            ("REMOTE SOURCE", "CREATE VIRTUAL TABLE"),
            ("REMOTE SOURCE", "REMOTE THING"),
        ]);
        let list = listed(&privileges, &object);
        grantable(&list);
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "SELECT",
                "DEBUG",
                "AGENT MESSAGING",
                "CATALOG READ",
                "CREATE X509 PROVIDER",
                "NEW THING",
                "REPO.EXPORT"
            ]
        );
        assert_eq!(
            on(&list, "SELECT"),
            Some(vec!["schema", "table", "view", "sequence"])
        );
        // The view says which kinds; schemas, which it covers, aren't added.
        assert_eq!(
            on(&list, "DEBUG"),
            Some(vec!["view", "procedure", "function"])
        );
        assert_eq!(on(&list, "AGENT MESSAGING"), Some(vec![""]));
        assert_eq!(
            on(&list, "NEW THING"),
            Some(vec![
                "schema",
                "table",
                "view",
                "procedure",
                "function",
                "sequence"
            ])
        );

        // Without SYS.OBJECT_PRIVILEGES (or with no rows for a kind), the
        // documented kinds.
        let list = listed(&privileges, &pairs(&[("TABLE", "SELECT")]));
        assert_eq!(
            on(&list, "SELECT"),
            Some(vec!["schema", "table", "view", "sequence"])
        );
        assert_eq!(
            on(&list, "DEBUG"),
            Some(vec!["schema", "view", "procedure", "function"])
        );
        assert_eq!(on(&list, "CREATE VIRTUAL TABLE"), None);
    }

    #[test]
    fn checks_privilege_names() {
        for ok in ["REPO.EXPORT", "create x509 provider", "SELECT", "_X.Y_1"] {
            assert!(valid(ok), "{ok}");
        }
        for bad in [
            "", " ", "REPO..X", ".X", "X.", "1A", "A-B", "A;B", "A'B", "A\"B", "A(B)", "A--B",
        ] {
            assert!(!valid(bad), "{bad}");
        }
        assert_eq!(
            script(&SecurityAction::Grant {
                privileges: vec!["repo.export".into()],
                object: None,
                to: "ANA".into(),
                grantable: false
            })
            .unwrap(),
            "GRANT REPO.EXPORT TO \"ANA\";"
        );
    }

    #[test]
    fn reads_granted_privileges() {
        let g = grant_of(
            "CATALOG READ".into(),
            "SYSTEMPRIVILEGE",
            None,
            None,
            None,
            true,
        );
        assert_eq!((g.object, g.object_kind, g.grantable), (None, None, true));
        let g = grant_of(
            "SELECT".into(),
            "SCHEMA",
            Some("VENTAS".into()),
            None,
            None,
            false,
        );
        assert_eq!(
            (g.object.as_deref(), g.object_kind.as_deref()),
            (Some("VENTAS"), Some("schema"))
        );
        let g = grant_of(
            "UPDATE".into(),
            "TABLE",
            Some("VENTAS".into()),
            Some("FACT".into()),
            Some("TOTAL".into()),
            false,
        );
        assert_eq!(
            (
                g.privilege.as_str(),
                g.object.as_deref(),
                g.object_kind.as_deref()
            ),
            ("UPDATE (TOTAL)", Some("VENTAS.FACT"), Some("table"))
        );
        let g = grant_of(
            "CREATE VIRTUAL TABLE".into(),
            "REMOTESOURCE",
            None,
            Some("HANA2".into()),
            None,
            false,
        );
        assert_eq!(
            (g.object.as_deref(), g.object_kind.as_deref()),
            (Some("HANA2"), Some("remotesource"))
        );
    }
}

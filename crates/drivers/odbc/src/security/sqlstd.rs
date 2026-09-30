//! Engines with SQL users and roles close to the standard's:
//!
//! - **MonetDB**: `sys.users`, `sys.auths` (users and roles),
//!   `sys.user_role` and `sys.privileges` (a bit mask per object).
//! - **Ingres**: `iiusers`, `iiroles`, `iirolegrants` (in iidbdb) and the
//!   database's `iiaccess`. GRANT names roles (`TO ROLE r`): `role:<name>`.
//! - **InterSystems IRIS / Caché**: the `%Library.SQLCatalogPriv` queries
//!   (users, roles, their privileges).
//! - **SAP MaxDB**: `DOMAIN.USERS`, `DOMAIN.ROLES`, `TABLEPRIVILEGES` and
//!   `SCHEMAPRIVILEGES`.
//! - **NuoDB**: `SYSTEM.USERS`, `ROLES`, `USERROLES` and `PRIVILEGES` (a bit
//!   mask, grantable bits 10 places up). Roles live in a schema and GRANT
//!   names them (`TO ROLE s.r`): `role:<schema.name>`.
//! - **HeavyDB**: `SHOW USER DETAILS`, `SHOW ROLES` and, connected to
//!   `information_schema`, its `permissions` table.
//! - **SQream DB**: only roles (`sqream_catalog.roles`); a role with LOGIN
//!   is shown as a user.

use super::{first, get, grantee, ident, lit, option, password, privileges, role, rows, unsupported_action, user, with_roles, Dialect, Row};
use crate::OdbcSession;
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashMap;

pub fn spec(d: Dialect) -> SecuritySpec {
    let base = SecuritySpec {
        privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "EXECUTE", "ALL"],
        object_kinds: vec![kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    };
    match d {
        Dialect::MonetDb => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "EXECUTE", "ALL", "COPY FROM", "COPY INTO"],
            // "" = global privileges (COPY FROM, COPY INTO).
            object_kinds: vec!["", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
            ..base
        },
        Dialect::Ingres => SecuritySpec { privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "COPY_INTO", "COPY_FROM", "EXECUTE", "ALL"], ..base },
        Dialect::Iris => SecuritySpec {
            privileges: vec![
                "SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "EXECUTE", "ALTER", "%CREATE_TABLE", "%ALTER_TABLE", "%DROP_TABLE",
                "%CREATE_VIEW", "%ALTER_VIEW", "%DROP_VIEW", "%CREATE_PROCEDURE", "%CREATE_FUNCTION", "%NOCHECK", "%NOLOCK", "%NOTRIGGER",
            ],
            // "" = administrative privileges (%CREATE_TABLE…).
            object_kinds: vec!["", "schema", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE],
            ..base
        },
        Dialect::MaxDb => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "INDEX", "REFERENCES", "EXECUTE", "ALL", "CREATEIN", "DROPIN", "ALTERIN"],
            object_kinds: vec!["schema", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE],
            ..base
        },
        Dialect::NuoDb => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "EXECUTE", "GRANT", "CREATE", "ALL"],
            object_kinds: vec!["schema", kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION],
            ..base
        },
        Dialect::HeavyDb => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "ALTER", "DROP", "ALL"],
            object_kinds: vec![kinds::TABLE, kinds::VIEW],
            ..base
        },
        _ => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "DDL", "ALL", "USAGE", "CREATE", "SUPERUSER", "CREATE FUNCTION"],
            // "" = SUPERUSER and the like.
            object_kinds: vec!["", "schema", kinds::TABLE, kinds::VIEW],
            ..base
        },
    }
}

fn members_by(rs: &[Row], member: &str, of: &str) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for r in rs {
        m.entry(get(r, member).to_ascii_lowercase()).or_default().push(get(r, of).to_string());
    }
    m
}

fn object(schema: &str, name: &str) -> Option<String> {
    (!name.is_empty()).then(|| if schema.is_empty() { name.to_string() } else { format!("{schema}.{name}") })
}

fn bits(mask: i64, table: &[(i64, &'static str)], grantable: impl Fn(i64) -> bool) -> Vec<(String, bool)> {
    table.iter().filter(|(b, _)| mask & b != 0).map(|(b, p)| (p.to_string(), grantable(*b))).collect()
}

// MonetDB ----------------------------------------------------------------------

const MONET_MEMBERS: &str = "SELECT u.name AS grantee, r.name AS role_name FROM sys.user_role ur JOIN sys.auths u ON u.id = ur.login_id JOIN sys.auths r ON r.id = ur.role_id";

pub async fn monet_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::MonetDb;
    let members = members_by(&rows(s, MONET_MEMBERS).await.unwrap_or_default(), "grantee", "role_name");
    let mut out = Vec::new();
    for u in rows(s, "SELECT u.name, u.fullname, s.name AS schema_name FROM sys.users u LEFT JOIN sys.schemas s ON s.id = u.default_schema").await? {
        let name = get(&u, "name").to_string();
        let member_of = members.get(&name.to_ascii_lowercase()).cloned().unwrap_or_default();
        let mut details = Vec::new();
        for (k, label) in [("fullname", "Nombre"), ("schema_name", "Esquema predeterminado")] {
            if !get(&u, k).is_empty() {
                details.push((label.to_string(), get(&u, k).to_string()));
            }
        }
        out.push(Principal {
            superuser: Some(name == "monetdb" || member_of.iter().any(|r| r == "sysadmin")),
            system: name == "monetdb" || name == ".snapshot",
            member_of,
            details,
            ..user(&name)
        });
    }
    for r in first(s, "SELECT name FROM sys.auths WHERE name NOT IN (SELECT name FROM sys.users)").await.unwrap_or_default() {
        out.push(Principal {
            member_of: members.get(&r.to_ascii_lowercase()).cloned().unwrap_or_default(),
            system: matches!(r.as_str(), "public" | "sysadmin" | "monetdb"),
            superuser: Some(r == "sysadmin"),
            ..role(d, &r)
        });
    }
    Ok(out)
}

const MONET_BITS: &[(i64, &str)] = &[(1, "SELECT"), (2, "UPDATE"), (4, "INSERT"), (8, "DELETE"), (16, "EXECUTE"), (32, "GRANT"), (64, "TRUNCATE")];

pub(super) fn monet_grants_of(r: &Row, via: &Option<String>) -> Vec<Grant> {
    let mask: i64 = get(r, "privileges").parse().unwrap_or(0);
    let (object, kind) = if let Some(t) = object(get(r, "tschema"), get(r, "tname")) {
        let view = matches!(get(r, "ttype"), "1" | "11");
        (Some(t), Some(if view { kinds::VIEW } else { kinds::TABLE }))
    } else if let Some(f) = object(get(r, "fschema"), get(r, "fname")) {
        let procedure = get(r, "ftype") == "2";
        (Some(f), Some(if procedure { kinds::PROCEDURE } else { kinds::FUNCTION }))
    } else {
        (None, None)
    };
    let known: i64 = MONET_BITS.iter().map(|(b, _)| b).sum();
    let mut names = bits(mask, MONET_BITS, |_| super::yes(get(r, "grantable")));
    if mask & !known != 0 {
        names.push((format!("PRIVILEGIO {}", mask & !known), super::yes(get(r, "grantable"))));
    }
    names
        .into_iter()
        .map(|(privilege, grantable)| Grant { privilege, object: object.clone(), object_kind: kind.map(str::to_string), grantable, denied: false, via: via.clone() })
        .collect()
}

pub async fn monet_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let all = rows(
        s,
        "SELECT a.name AS grantee, p.privileges, p.grantable, t.name AS tname, st.name AS tschema, t.type AS ttype,
                f.name AS fname, sf.name AS fschema, f.type AS ftype
           FROM sys.privileges p JOIN sys.auths a ON a.id = p.auth_id
           LEFT JOIN sys.tables t ON t.id = p.obj_id LEFT JOIN sys.schemas st ON st.id = t.schema_id
           LEFT JOIN sys.functions f ON f.id = p.obj_id LEFT JOIN sys.schemas sf ON sf.id = f.schema_id",
    )
    .await?;
    let members = members_by(&rows(s, MONET_MEMBERS).await.unwrap_or_default(), "grantee", "role_name");
    Ok(with_roles(grantee(principal).1, &members, |n, via| all.iter().filter(|r| get(r, "grantee") == n).flat_map(|r| monet_grants_of(r, &via)).collect()))
}

// Ingres -----------------------------------------------------------------------

pub async fn ingres_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Ingres;
    let grants = rows(s, "SELECT role_name, gr_type, grantee_name FROM iirolegrants").await.unwrap_or_default();
    let of = |n: &str| -> Vec<String> {
        grants.iter().filter(|g| get(g, "grantee_name") == n && get(g, "gr_type") == "U").map(|g| super::role_name(d, get(g, "role_name"))).collect()
    };
    let mut out = Vec::new();
    match rows(s, "SELECT * FROM iiusers").await {
        Ok(users) => {
            for u in users {
                let name = get(&u, "user_name").to_string();
                let mut details = Vec::new();
                for (k, label) in [("default_group", "Grupo predeterminado"), ("profile_name", "Perfil"), ("expire_date", "Vence")] {
                    if !get(&u, k).is_empty() {
                        details.push((label.to_string(), get(&u, k).to_string()));
                    }
                }
                out.push(Principal {
                    superuser: Some(super::yes(get(&u, "security")) || super::yes(get(&u, "maintain_users"))),
                    system: name == "$ingres" || name == "ingres",
                    member_of: of(&name),
                    details,
                    ..user(&name)
                });
            }
        }
        // iiusers lives in iidbdb: elsewhere, whoever has a permit.
        Err(_) => {
            for n in first(s, "SELECT DISTINCT permit_user FROM iiaccess").await? {
                if n != "$public" {
                    out.push(Principal { can_login: None, member_of: of(&n), ..user(&n) });
                }
            }
        }
    }
    for r in first(s, "SELECT role_name FROM iiroles").await.unwrap_or_default() {
        out.push(role(d, &r));
    }
    Ok(out)
}

pub async fn ingres_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let d = Dialect::Ingres;
    let (is_role, name) = grantee(principal);
    let all = rows(s, "SELECT table_name, table_owner, table_type, permit_user, permit_type FROM iiaccess").await?;
    let of = |n: &str, via: Option<String>| -> Vec<Grant> {
        all.iter()
            .filter(|r| get(r, "permit_user") == n)
            .map(|r| Grant {
                privilege: get(r, "permit_type").to_uppercase(),
                object: object(get(r, "table_owner"), get(r, "table_name")),
                object_kind: Some(if get(r, "table_type") == "V" { kinds::VIEW } else { kinds::TABLE }.into()),
                via: via.clone(),
                ..Default::default()
            })
            .collect()
    };
    let mut out = of(name, None);
    if !is_role {
        for g in rows(s, &format!("SELECT role_name FROM iirolegrants WHERE grantee_name = {}", lit(name))).await.unwrap_or_default() {
            let r = get(&g, "role_name");
            out.extend(of(r, Some(super::role_name(d, r))));
        }
    }
    Ok(out)
}

// IRIS / Caché -----------------------------------------------------------------

fn iris_kind(t: &str) -> &'static str {
    let t = t.to_ascii_uppercase();
    if t.contains("VIEW") {
        kinds::VIEW
    } else if t.contains("PROCEDURE") {
        kinds::PROCEDURE
    } else if t.contains("SCHEMA") {
        "schema"
    } else {
        kinds::TABLE
    }
}

pub async fn iris_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Iris;
    let mut out = Vec::new();
    for u in rows(s, "CALL %Library.SQLCatalogPriv_SQLUsers()").await? {
        let name = get(&u, "username").to_string();
        let member_of: Vec<String> = rows(s, &format!("CALL %Library.SQLCatalogPriv_SQLUserRole({})", lit(&name)))
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| get(r, "role_name").to_string())
            .collect();
        let mut details = Vec::new();
        if !get(&u, "description").is_empty() {
            details.push(("Descripción".to_string(), get(&u, "description").to_string()));
        }
        out.push(Principal {
            superuser: Some(member_of.iter().any(|r| r == "%All")),
            disabled: Some(!get(&u, "enabled").is_empty() && !super::yes(get(&u, "enabled"))),
            system: name.starts_with('_') || matches!(name.as_str(), "SuperUser" | "Admin" | "CSPSystem" | "UnknownUser" | "IAM"),
            member_of,
            details,
            ..user(&name)
        });
    }
    for r in rows(s, "CALL %Library.SQLCatalogPriv_SQLRoles()").await.unwrap_or_default() {
        let name = get(&r, "role_name");
        out.push(Principal { system: name.starts_with('%'), superuser: Some(name == "%All"), ..role(d, name) });
    }
    Ok(out)
}

pub async fn iris_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let n = lit(grantee(principal).1);
    let is_role = rows(s, "CALL %Library.SQLCatalogPriv_SQLRoles()").await.unwrap_or_default().iter().any(|r| get(r, "role_name") == grantee(principal).1);
    let via = |r: &Row| Some(get(r, "granted_via").to_string()).filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("direct"));
    let obj = if is_role { format!("CALL %Library.SQLCatalogPriv_SQLRolePrivileges({n})") } else { format!("CALL %Library.SQLCatalogPriv_SQLUserPrivs({n})") };
    let mut out: Vec<Grant> = rows(s, &obj)
        .await?
        .iter()
        .map(|r| Grant {
            privilege: get(r, "privilege").to_string(),
            object: Some(get(r, "name").to_string()),
            object_kind: Some(iris_kind(get(r, "type")).into()),
            grantable: super::yes(get(r, "grant_option")),
            denied: false,
            via: via(r),
        })
        .collect();
    if !is_role {
        for r in rows(s, &format!("CALL %Library.SQLCatalogPriv_SQLUserSysPrivs({n})")).await.unwrap_or_default() {
            out.push(Grant { privilege: get(&r, "privilege").to_string(), grantable: super::yes(get(&r, "admin_option")), via: via(&r), ..Default::default() });
        }
    }
    Ok(out)
}

// MaxDB ------------------------------------------------------------------------

pub async fn maxdb_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::MaxDb;
    let mut out = Vec::new();
    for u in rows(s, "SELECT * FROM DOMAIN.USERS").await? {
        let name = get(&u, "username").to_string();
        let mode = get(&u, "usermode").to_string();
        let mut details = vec![("Tipo".to_string(), mode.clone())];
        if !get(&u, "groupname").is_empty() {
            details.push(("Grupo".into(), get(&u, "groupname").to_string()));
        }
        out.push(Principal {
            superuser: Some(mode == "SYSDBA" || mode == "DBA"),
            system: mode == "SYSDBA" || name == "DOMAIN" || name == "SYS",
            details,
            ..user(&name)
        });
    }
    for r in first(s, "SELECT DISTINCT ROLE FROM DOMAIN.ROLES").await.unwrap_or_default() {
        out.push(role(d, &r));
    }
    Ok(out)
}

pub async fn maxdb_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let n = lit(grantee(principal).1);
    let mut out = Vec::new();
    for r in rows(s, &format!("SELECT USERMODE FROM DOMAIN.USERS WHERE USERNAME = {n}")).await.unwrap_or_default() {
        out.push(Grant { privilege: get(&r, "usermode").to_string(), ..Default::default() });
    }
    for r in rows(s, &format!("SELECT SCHEMANAME, TABLENAME, PRIVILEGE, IS_GRANTABLE FROM DOMAIN.TABLEPRIVILEGES WHERE GRANTEE = {n}")).await? {
        out.push(Grant {
            privilege: get(&r, "privilege").to_string(),
            object: object(get(&r, "schemaname"), get(&r, "tablename")),
            object_kind: Some(kinds::TABLE.into()),
            grantable: super::yes(get(&r, "is_grantable")),
            ..Default::default()
        });
    }
    for r in rows(s, &format!("SELECT * FROM DOMAIN.SCHEMAPRIVILEGES WHERE GRANTEE = {n}")).await.unwrap_or_default() {
        for p in ["createin", "dropin", "alterin"] {
            if super::yes(get(&r, p)) || get(&r, p).eq_ignore_ascii_case("YES") {
                out.push(Grant {
                    privilege: p.to_uppercase(),
                    object: Some(get(&r, "schemaname").to_string()),
                    object_kind: Some("schema".into()),
                    grantable: super::yes(get(&r, "grantoption")),
                    ..Default::default()
                });
            }
        }
    }
    Ok(out)
}

// NuoDB ------------------------------------------------------------------------

const NUO_BITS: &[(i64, &str)] =
    &[(2, "SELECT"), (4, "INSERT"), (8, "UPDATE"), (16, "DELETE"), (32, "GRANT"), (64, "ALTER"), (128, "EXECUTE"), (256, "TRIGGERS"), (512, "PROCEDURES"), (1024, "CREATE")];

fn nuo_kind(t: &str) -> &'static str {
    match t.trim() {
        "1" => kinds::VIEW,
        "2" => kinds::PROCEDURE,
        "6" => "sequence",
        "9" => kinds::FUNCTION,
        "10" => "schema",
        _ => kinds::TABLE,
    }
}

pub(super) fn nuo_grants_of(r: &Row, via: &Option<String>) -> Vec<Grant> {
    let mask: i64 = get(r, "privilegemask").parse().unwrap_or(0);
    let kind = nuo_kind(get(r, "objecttype"));
    let obj = if kind == "schema" { Some(get(r, "objectname").to_string()) } else { object(get(r, "objectschema"), get(r, "objectname")) };
    let names = if mask == -1 { vec![("ALL (dueño)".to_string(), true)] } else { bits(mask, NUO_BITS, |b| mask & (b << 10) != 0) };
    names.into_iter().map(|(privilege, grantable)| Grant { privilege, object: obj.clone(), object_kind: Some(kind.into()), grantable, denied: false, via: via.clone() }).collect()
}

pub async fn nuo_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::NuoDb;
    let links = rows(s, "SELECT USERNAME, ROLESCHEMA, ROLENAME FROM SYSTEM.USERROLES").await.unwrap_or_default();
    let of = |n: &str| -> Vec<String> {
        links.iter().filter(|l| get(l, "username") == n).map(|l| super::role_name(d, &format!("{}.{}", get(l, "roleschema"), get(l, "rolename")))).collect()
    };
    let mut out = Vec::new();
    for u in first(s, "SELECT USERNAME FROM SYSTEM.USERS").await? {
        let member_of = of(&u);
        out.push(Principal {
            superuser: Some(member_of.iter().any(|r| r.ends_with("SYSTEM.DBA") || r.ends_with("SYSTEM.ADMINISTRATOR"))),
            system: u == "DBA" || u == "CLOUD",
            member_of,
            ..user(&u)
        });
    }
    for r in rows(s, "SELECT SCHEMA, ROLENAME FROM SYSTEM.ROLES").await.unwrap_or_default() {
        let full = format!("{}.{}", get(&r, "schema"), get(&r, "rolename"));
        out.push(Principal { system: get(&r, "schema") == "SYSTEM", superuser: Some(full == "SYSTEM.DBA"), ..role(d, &full) });
    }
    Ok(out)
}

pub async fn nuo_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let d = Dialect::NuoDb;
    let all = rows(s, "SELECT * FROM SYSTEM.PRIVILEGES").await?;
    let (is_role, name) = grantee(principal);
    let held = |schema: &str, n: &str, role: bool| {
        all.iter()
            .filter(|r| get(r, "holdername") == n && (!role || get(r, "holderschema") == schema) && (get(r, "holdertype") == "4") == role)
            .cloned()
            .collect::<Vec<_>>()
    };
    let split = |r: &str| r.split_once('.').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or_else(|| (String::new(), r.to_string()));
    let mut out = Vec::new();
    if is_role {
        let (sc, n) = split(name);
        out.extend(held(&sc, &n, true).iter().flat_map(|r| nuo_grants_of(r, &None)));
    } else {
        out.extend(held("", name, false).iter().flat_map(|r| nuo_grants_of(r, &None)));
        for l in rows(s, &format!("SELECT ROLESCHEMA, ROLENAME FROM SYSTEM.USERROLES WHERE USERNAME = {}", lit(name))).await.unwrap_or_default() {
            let (sc, n) = (get(&l, "roleschema"), get(&l, "rolename"));
            let via = Some(super::role_name(d, &format!("{sc}.{n}")));
            out.extend(held(sc, n, true).iter().flat_map(|r| nuo_grants_of(r, &via)));
        }
    }
    Ok(out)
}

/// `schema.role`, each part as an identifier.
fn nuo_role(r: &str) -> String {
    let d = Dialect::NuoDb;
    match r.split_once('.') {
        Some((s, n)) => format!("{}.{}", ident(d, s), ident(d, n)),
        None => ident(d, r),
    }
}

// HeavyDB ----------------------------------------------------------------------

pub async fn heavy_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::HeavyDb;
    let mut out = Vec::new();
    for u in rows(s, "SHOW USER DETAILS").await? {
        let name = get(&u, "name").to_string();
        let mut details = Vec::new();
        if !get(&u, "default_db").is_empty() {
            details.push(("Base predeterminada".to_string(), get(&u, "default_db").to_string()));
        }
        let can_login = get(&u, "can_login");
        out.push(Principal {
            superuser: Some(super::yes(get(&u, "is_super"))),
            can_login: Some(can_login.is_empty() || super::yes(can_login)),
            system: name == "admin",
            details,
            ..user(&name)
        });
    }
    for r in first(s, "SHOW ROLES").await.unwrap_or_default() {
        out.push(role(d, &r));
    }
    Ok(out)
}

pub async fn heavy_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let sql = format!(
        "SELECT object_name, object_permission_type, object_permissions, database_name FROM information_schema.permissions WHERE role_name = {}",
        lit(grantee(principal).1)
    );
    let rs = rows(s, &sql)
        .await
        .map_err(|e| Error::Query(format!("HeavyDB muestra los permisos solo conectado a la base information_schema ({e})")))?;
    let mut out = Vec::new();
    for r in rs {
        let kind = if get(&r, "object_permission_type").to_ascii_lowercase().contains("view") { kinds::VIEW } else { kinds::TABLE };
        for p in get(&r, "object_permissions").trim_matches(|c| c == '{' || c == '}' || c == '[' || c == ']').split(',') {
            let p = p.trim().trim_matches('"').trim();
            if !p.is_empty() {
                out.push(Grant { privilege: p.to_uppercase(), object: object(get(&r, "database_name"), get(&r, "object_name")), object_kind: Some(kind.into()), ..Default::default() });
            }
        }
    }
    Ok(out)
}

// SQream -----------------------------------------------------------------------

async fn sqream_links(s: &OdbcSession) -> Vec<Row> {
    for t in ["roles_memberships", "role_memberships", "roles_roles"] {
        let sql = format!(
            "SELECT r.name AS grp, m.name AS member FROM sqream_catalog.{t} x
               JOIN sqream_catalog.roles r ON r.role_id = x.role_id JOIN sqream_catalog.roles m ON m.role_id = x.member_role_id"
        );
        if let Ok(r) = rows(s, &sql).await {
            return r;
        }
    }
    Vec::new()
}

pub async fn sqream_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Sqream;
    let links = sqream_links(s).await;
    let mut out = Vec::new();
    for r in rows(s, "SELECT * FROM sqream_catalog.roles").await? {
        let name = get(&r, "name").to_string();
        let member_of: Vec<String> = links.iter().filter(|l| get(l, "member") == name).map(|l| get(l, "grp").to_string()).collect();
        let login = super::yes(get(&r, "login"));
        let base = if login { user(&name) } else { role(d, &name) };
        out.push(Principal {
            superuser: Some(super::yes(get(&r, "superuser"))),
            can_login: Some(login),
            system: name == "public" || name == "sqream",
            member_of,
            ..base
        });
    }
    Ok(out)
}

pub async fn sqream_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let links = sqream_links(s).await;
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for l in &links {
        members.entry(get(l, "member").to_ascii_lowercase()).or_default().push(get(l, "grp").to_string());
    }
    let tables = rows(
        s,
        "SELECT r.name AS grantee, t.schema_name, t.table_name, pt.name AS privilege
           FROM sqream_catalog.table_permissions p JOIN sqream_catalog.roles r ON r.role_id = p.role_id
           JOIN sqream_catalog.permission_types pt ON pt.permission_type_id = p.permission_type
           LEFT JOIN sqream_catalog.tables t ON t.table_id = p.table_id",
    )
    .await?;
    let schemas = rows(
        s,
        "SELECT r.name AS grantee, sc.schema_name, pt.name AS privilege
           FROM sqream_catalog.schema_permissions p JOIN sqream_catalog.roles r ON r.role_id = p.role_id
           JOIN sqream_catalog.permission_types pt ON pt.permission_type_id = p.permission_type
           LEFT JOIN sqream_catalog.schemas sc ON sc.schema_id = p.schema_id",
    )
    .await
    .unwrap_or_default();
    let dbs = rows(
        s,
        "SELECT r.name AS grantee, p.database_name, pt.name AS privilege
           FROM sqream_catalog.database_permissions p JOIN sqream_catalog.roles r ON r.role_id = p.role_id
           JOIN sqream_catalog.permission_types pt ON pt.permission_type_id = p.permission_type",
    )
    .await
    .unwrap_or_default();
    Ok(with_roles(grantee(principal).1, &members, |n, via| {
        let mut out = Vec::new();
        for r in tables.iter().filter(|r| get(r, "grantee") == n) {
            out.push(Grant { privilege: get(r, "privilege").to_uppercase(), object: object(get(r, "schema_name"), get(r, "table_name")), object_kind: Some(kinds::TABLE.into()), via: via.clone(), ..Default::default() });
        }
        for r in schemas.iter().filter(|r| get(r, "grantee") == n) {
            out.push(Grant { privilege: get(r, "privilege").to_uppercase(), object: Some(get(r, "schema_name").to_string()), object_kind: Some("schema".into()), via: via.clone(), ..Default::default() });
        }
        for r in dbs.iter().filter(|r| get(r, "grantee") == n) {
            out.push(Grant { privilege: get(r, "privilege").to_uppercase(), object: Some(get(r, "database_name").to_string()), object_kind: Some("database".into()), via: via.clone(), ..Default::default() });
        }
        out
    }))
}

// scripts ----------------------------------------------------------------------

fn on(d: Dialect, o: &ObjectRef) -> String {
    let q = super::qualified(d, o);
    let k = o.kind.as_str();
    match d {
        _ if k == "schema" => match d {
            Dialect::MaxDb => format!(" ON {}", ident(d, &o.name)),
            _ => format!(" ON SCHEMA {}", ident(d, &o.name)),
        },
        Dialect::MonetDb | Dialect::NuoDb | Dialect::Ingres if k == kinds::PROCEDURE => format!(" ON PROCEDURE {q}"),
        Dialect::MonetDb | Dialect::NuoDb if k == kinds::FUNCTION => format!(" ON FUNCTION {q}"),
        Dialect::HeavyDb if k == kinds::VIEW => format!(" ON VIEW {q}"),
        Dialect::NuoDb | Dialect::Ingres | Dialect::HeavyDb | Dialect::Sqream => format!(" ON TABLE {q}"),
        _ => format!(" ON {q}"),
    }
}

/// Passwords that are identifiers (MaxDB, IRIS): in double quotes.
fn quoted_pw(p: &str) -> String {
    format!("\"{}\"", p.replace('"', "\"\""))
}

const NO_LOCK: &str = "este motor no bloquea ni deshabilita usuarios desde SQL: cambiale la contraseña o quitale los permisos";

pub fn script(d: Dialect, a: &SecurityAction) -> Result<String> {
    let id = |n: &str| match d {
        Dialect::NuoDb if grantee(n).0 => nuo_role(grantee(n).1),
        _ => ident(d, grantee(n).1),
    };
    let role_id = |n: &str| if d == Dialect::NuoDb { nuo_role(grantee(n).1) } else { ident(d, grantee(n).1) };
    let to_whom = |n: &str| match (d, grantee(n)) {
        (Dialect::Ingres | Dialect::NuoDb, (true, _)) => format!("ROLE {}", role_id(n)),
        _ => id(n),
    };
    Ok(match a {
        SecurityAction::CreateUser { name, password: p } => {
            let p = password(p)?;
            let n = id(name);
            match d {
                Dialect::MonetDb => format!("CREATE USER {n} WITH PASSWORD {} NAME {} SCHEMA \"sys\";", lit(p), lit(name)),
                Dialect::Ingres => format!("CREATE USER {n} WITH PASSWORD = {};", lit(p)),
                Dialect::Iris => format!("CREATE USER {n} IDENTIFY BY {};", quoted_pw(p)),
                Dialect::MaxDb => format!("CREATE USER {n} PASSWORD {} STANDARD;", quoted_pw(p)),
                Dialect::NuoDb => format!("CREATE USER {n} PASSWORD {};", lit(p)),
                Dialect::HeavyDb => format!("CREATE USER {n} (password = {});", lit(p)),
                _ => format!("CREATE ROLE {n};\nGRANT LOGIN TO {n};\nGRANT PASSWORD {} TO {n};", lit(p)),
            }
        }
        SecurityAction::SetPassword { name, password: p } => {
            let n = id(name);
            match d {
                Dialect::MonetDb => format!("ALTER USER {n} WITH PASSWORD {};", lit(p)),
                Dialect::Ingres => format!("ALTER USER {n} WITH PASSWORD = {};", lit(p)),
                Dialect::Iris => format!("ALTER USER {n} IDENTIFY BY {};", quoted_pw(p)),
                Dialect::MaxDb => format!("ALTER PASSWORD {n} {};", quoted_pw(p)),
                Dialect::NuoDb => format!("ALTER USER {n} PASSWORD {};", lit(p)),
                Dialect::HeavyDb => format!("ALTER USER {n} (password = {});", lit(p)),
                _ => format!("GRANT PASSWORD {} TO {n};", lit(p)),
            }
        }
        SecurityAction::SetLogin { name, enabled } => match d {
            Dialect::MaxDb => format!("ALTER USER {} {} CONNECT;", id(name), if *enabled { "ENABLE" } else { "DISABLE" }),
            Dialect::HeavyDb => format!("ALTER USER {} (can_login = '{}');", id(name), enabled),
            Dialect::Sqream if *enabled => format!("GRANT LOGIN TO {};", id(name)),
            Dialect::Sqream => format!("REVOKE LOGIN FROM {};", id(name)),
            _ => return unsupported_action(NO_LOCK),
        },
        SecurityAction::Drop { name, kind: PrincipalKind::User } => match d {
            Dialect::Sqream => format!("DROP ROLE {};", id(name)),
            _ => format!("DROP USER {};", id(name)),
        },
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", role_id(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", role_id(name)),
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", role_id(role), id(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", role_id(role), id(member)),
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            let on = match object {
                Some(o) => on(d, o),
                None if matches!(d, Dialect::MonetDb | Dialect::Iris | Dialect::Sqream) => String::new(),
                None => return Err(Error::Query("elegí un esquema, una tabla, una vista o una rutina: este motor no tiene permisos del sistema por GRANT".into())),
            };
            let option = match (d, grantable, object) {
                (_, false, _) => "",
                (Dialect::HeavyDb | Dialect::Sqream, true, _) => return unsupported_action("este motor no otorga permisos con opción de otorgarlos a otros"),
                (Dialect::Iris, true, None) => " WITH ADMIN OPTION",
                _ => option(true),
            };
            format!("GRANT {}{on} TO {}{option};", privileges(p)?, to_whom(to))
        }
        SecurityAction::Revoke { privileges: p, object, from } => {
            let on = object.as_ref().map(|o| on(d, o)).unwrap_or_default();
            format!("REVOKE {}{on} FROM {};", privileges(p)?, to_whom(from))
        }
    })
}

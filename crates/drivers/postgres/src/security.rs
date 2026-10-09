//! Users, roles and permissions (docs/users-and-permissions.md). Roles are
//! the server's (or the cluster's), not a database's; privileges are read
//! for the session's database.
//!
//! - PostgreSQL and the engines that keep its catalog: `pg_roles`,
//!   `pg_auth_members` and the ACL columns (`relacl`, `nspacl`, `datacl`,
//!   `proacl`) through `aclexplode`.
//! - CockroachDB: `pg_roles` / `pg_auth_members` and `SHOW GRANTS FOR`.
//! - Redshift: users (`pg_user`) and groups (`pg_group`), privileges from
//!   the `svv_*_privileges` views. Groups are named `GROUP <name>`: that's
//!   how Redshift names them as grantees, and the scripts need to tell them
//!   from users.
//! - Materialize: `pg_roles` / `pg_auth_members` and `SHOW PRIVILEGES
//!   FOR`; server-wide grants are its `SYSTEM` privileges.
//! - CrateDB: `sys.users`, `sys.roles` and `sys.privileges`.
//! - H2 (`-pg` server mode): its own `INFORMATION_SCHEMA` (`security/h2.rs`).
//! - RisingWave: users only, ACLs in `rw_catalog` (`security/risingwave.rs`).

mod h2;
mod risingwave;

use crate::catalog::{cell, lit};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashMap;

/// How an engine manages its users.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Pg,
    Cockroach,
    Redshift,
    Materialize,
    Crate,
    H2,
    RisingWave,
}

fn dialect(v: Variant) -> Option<Dialect> {
    match v {
        Variant::Cockroach => Some(Dialect::Cockroach),
        Variant::Redshift => Some(Dialect::Redshift),
        Variant::CrateDb => Some(Dialect::Crate),
        Variant::Materialize => Some(Dialect::Materialize),
        Variant::H2 => Some(Dialect::H2),
        Variant::RisingWave => Some(Dialect::RisingWave),
        // Denodo manages users in its own server (VQL).
        Variant::Denodo => None,
        _ => Some(Dialect::Pg),
    }
}

fn unsupported() -> Error {
    Error::Unsupported("este motor no administra usuarios desde DBine".into())
}

pub fn spec(v: Variant) -> Option<SecuritySpec> {
    let d = dialect(v)?;
    let listed: Vec<&str> = v.info().object_kinds.iter().map(|k| k.id).collect();
    let kinds = |all: &[&'static str]| -> Vec<&'static str> {
        all.iter().copied().filter(|k| k.is_empty() || *k == "schema" || listed.contains(k)).collect()
    };
    Some(match d {
        Dialect::Pg => SecuritySpec {
            privileges: vec![
                "SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "USAGE", "EXECUTE", "CREATE",
                "CONNECT", "TEMPORARY", "ALL PRIVILEGES",
            ],
            object_kinds: kinds(&["", "schema", "table", "view", "materialized_view", "procedure", "function"]),
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
        Dialect::Cockroach => SecuritySpec {
            privileges: vec![
                "SELECT", "INSERT", "UPDATE", "DELETE", "USAGE", "EXECUTE", "CREATE", "DROP", "CONNECT", "ZONECONFIG", "ALL",
            ],
            // No database-wide grants: they need the database's name, and
            // CockroachDB's DO blocks can't run dynamic SQL to look it up.
            object_kinds: kinds(&["schema", "table", "view", "materialized_view", "procedure", "function"]),
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
        Dialect::Redshift => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "USAGE", "CREATE", "TEMPORARY", "ALL"],
            // Database-wide grants need the database's name, which a script
            // can't look up on Redshift (no DO blocks); functions need their
            // argument types.
            object_kinds: vec!["schema", "table", "view"],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
        Dialect::Materialize => SecuritySpec {
            privileges: vec![
                "SELECT", "INSERT", "UPDATE", "DELETE", "USAGE", "CREATE", "ALL PRIVILEGES", "CREATEROLE", "CREATEDB",
                "CREATECLUSTER", "CREATENETWORKPOLICY",
            ],
            // "": the SYSTEM privileges (CREATEROLE, CREATEDB…).
            object_kinds: kinds(&["", "schema", "table", "view", "materialized_view", "source"]),
            create_user: true,
            create_role: true,
            // Only self-managed Materialize with password authentication
            // takes them; elsewhere the server says so.
            passwords: true,
            membership: true,
            per_database: false,
        },
        Dialect::Crate => SecuritySpec {
            privileges: vec!["DQL", "DML", "DDL", "AL"],
            object_kinds: vec!["", "schema", "table", "view"],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
        Dialect::H2 => h2::spec(),
        Dialect::RisingWave => risingwave::spec(kinds(&["schema", "table", "view", "materialized_view", "source", "sink"])),
    })
}

// -- reading ---------------------------------------------------------------

fn yes(r: &tokio_postgres::SimpleQueryRow, name: &str) -> bool {
    matches!(cell(r, name).as_deref(), Some("t" | "true" | "1"))
}

/// Roles every service creates for its administrators and agents.
const SERVICE_PREFIXES: &[&str] = &["pg_", "rds", "cloudsql", "alloydb", "azure_", "yb_", "gp_"];
/// Membership that makes an administrator where there's no real superuser.
const SERVICE_ADMINS: &[&str] = &["rds_superuser", "cloudsqlsuperuser", "alloydbsuperuser", "azure_pg_admin", "yb_superuser"];

/// (member, role) pairs.
async fn memberships(s: &PgSession, d: Dialect) -> Vec<(String, String)> {
    let sql = match d {
        Dialect::Pg | Dialect::Cockroach | Dialect::Materialize => {
            "SELECT m.rolname AS member, r.rolname AS role
               FROM pg_auth_members am
               JOIN pg_roles r ON r.oid = am.roleid
               JOIN pg_roles m ON m.oid = am.member"
        }
        Dialect::Redshift => {
            "SELECT u.usename AS member, 'GROUP ' || g.groname AS role
               FROM pg_group g, pg_user u WHERE u.usesysid = ANY(g.grolist)"
        }
        Dialect::Crate => {
            "SELECT name AS member, unnest(granted_roles['role']) AS role FROM sys.users
             UNION ALL
             SELECT name AS member, unnest(granted_roles['role']) AS role FROM sys.roles"
        }
        Dialect::H2 => h2::MEMBERSHIPS,
        Dialect::RisingWave => return Vec::new(),
    };
    match s.text(sql).await {
        Ok(rows) => rows.iter().filter_map(|r| Some((cell(r, "member")?, cell(r, "role")?))).collect(),
        Err(e) => {
            tracing::debug!("{:?}: role memberships unavailable: {e}", s.variant);
            Vec::new()
        }
    }
}

/// The roles `principal` belongs to, directly or not, each with the direct
/// role it comes through.
fn closure(principal: &str, members: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut queue: Vec<(String, String)> =
        members.iter().filter(|(m, _)| m == principal).map(|(_, r)| (r.clone(), r.clone())).collect();
    while let Some((role, via)) = queue.pop() {
        if role == principal || out.iter().any(|(r, _)| *r == role) {
            continue;
        }
        queue.extend(members.iter().filter(|(m, _)| *m == role).map(|(_, r)| (r.clone(), via.clone())));
        out.push((role, via));
    }
    out
}

pub async fn principals(s: &PgSession) -> Result<Vec<Principal>> {
    let d = dialect(s.variant).ok_or_else(unsupported)?;
    let mut out = match d {
        Dialect::Pg | Dialect::Cockroach | Dialect::Materialize => pg_principals(s, d).await?,
        Dialect::Redshift => redshift_principals(s).await?,
        Dialect::Crate => crate_principals(s).await?,
        Dialect::H2 => h2::principals(s).await?,
        Dialect::RisingWave => risingwave::principals(s).await?,
    };
    for (member, role) in memberships(s, d).await {
        if let Some(p) = out.iter_mut().find(|p| p.name == member) {
            p.member_of.push(role);
        }
    }
    for p in out.iter_mut() {
        p.member_of.sort();
        if s.variant.managed() || matches!(s.variant, Variant::Yugabyte) {
            if let Some(admin) = p.member_of.iter().find(|r| SERVICE_ADMINS.contains(&r.as_str())) {
                p.details.push(("Administrador del servicio".into(), format!("sí, por {admin}")));
            }
        }
    }
    Ok(out)
}

async fn pg_principals(s: &PgSession, d: Dialect) -> Result<Vec<Principal>> {
    // A NOLOGIN role with a password is a disabled user; only a superuser
    // can see passwords (pg_authid); CockroachDB reports one for all and
    // Materialize has no pg_authid.
    let query = |password: &str, join: &str| {
        format!(
            "SELECT r.oid::text AS oid, r.rolname AS name, r.rolcanlogin AS login, r.rolsuper AS super,
                    r.rolcreatedb AS createdb, r.rolcreaterole AS createrole, r.rolconnlimit::text AS connlimit,
                    r.rolvaliduntil::text AS valid,
                    (r.rolvaliduntil IS NOT NULL AND r.rolvaliduntil < now()) AS expired,
                    {password} AS pw
               FROM pg_roles r {join}
              ORDER BY r.rolname"
        )
    };
    let with_password = query("a.rolpassword IS NOT NULL", "LEFT JOIN pg_authid a ON a.oid = r.oid");
    let without = query("false", "");
    let rows = match d {
        Dialect::Cockroach | Dialect::Materialize => s.text(&without).await?,
        _ => match s.text(&with_password).await {
            Ok(r) => r,
            Err(_) => s.text(&without).await?,
        },
    };
    Ok(rows
        .iter()
        .map(|r| {
            let name = cell(r, "name").unwrap_or_default();
            let login = yes(r, "login");
            let user = login || yes(r, "pw");
            let oid: u64 = cell(r, "oid").and_then(|o| o.parse().ok()).unwrap_or(u64::MAX);
            let system = match d {
                Dialect::Cockroach => matches!(name.as_str(), "root" | "admin" | "node" | "public"),
                Dialect::Materialize => name.starts_with("mz_"),
                _ => oid < 16384 || SERVICE_PREFIXES.iter().any(|p| name.starts_with(p)),
            };
            let mut details = vec![(
                "Tipo".to_string(),
                if user { "Usuario (rol con ingreso)" } else { "Rol (sin ingreso)" }.to_string(),
            )];
            if yes(r, "createdb") {
                details.push(("Puede crear bases".into(), "sí".into()));
            }
            if yes(r, "createrole") {
                details.push(("Puede crear roles".into(), "sí".into()));
            }
            if let Some(n) = cell(r, "connlimit").filter(|n| n != "-1") {
                details.push(("Límite de conexiones".into(), n));
            }
            if let Some(v) = cell(r, "valid").filter(|v| v != "infinity") {
                details.push(("Vence".into(), v));
            }
            Principal {
                kind: if user { PrincipalKind::User } else { PrincipalKind::Role },
                can_login: Some(login),
                superuser: Some(yes(r, "super")),
                disabled: user.then(|| !login || yes(r, "expired")),
                member_of: Vec::new(),
                details,
                system,
                name,
            }
        })
        .collect())
}

async fn redshift_principals(s: &PgSession) -> Result<Vec<Principal>> {
    let users = s
        .text(
            "SELECT usename AS name, usesuper AS super, usecreatedb AS createdb, valuntil::text AS valid,
                    usesysid::text AS id
               FROM pg_user ORDER BY usename",
        )
        .await?;
    let mut out: Vec<Principal> = users
        .iter()
        .map(|r| {
            let name = cell(r, "name").unwrap_or_default();
            let mut details = vec![("Tipo".to_string(), "Usuario".to_string())];
            if yes(r, "createdb") {
                details.push(("Puede crear bases".into(), "sí".into()));
            }
            if let Some(v) = cell(r, "valid").filter(|v| v != "infinity") {
                details.push(("Vence".into(), v));
            }
            Principal {
                kind: PrincipalKind::User,
                can_login: Some(true),
                superuser: Some(yes(r, "super")),
                // Redshift can't tell a disabled password from a set one.
                disabled: None,
                member_of: Vec::new(),
                details,
                system: name == "rdsdb" || name.starts_with("rds") || cell(r, "id").as_deref() == Some("1"),
                name,
            }
        })
        .collect();
    if let Ok(groups) = s.text("SELECT groname AS name FROM pg_group ORDER BY groname").await {
        out.extend(groups.iter().filter_map(|r| cell(r, "name")).map(|g| Principal {
            name: format!("GROUP {g}"),
            kind: PrincipalKind::Role,
            can_login: Some(false),
            details: vec![("Tipo".into(), "Grupo".into())],
            ..Default::default()
        }));
    }
    Ok(out)
}

async fn crate_principals(s: &PgSession) -> Result<Vec<Principal>> {
    let users = s.text("SELECT name, superuser AS super FROM sys.users ORDER BY name").await?;
    let mut out: Vec<Principal> = users
        .iter()
        .map(|r| {
            let name = cell(r, "name").unwrap_or_default();
            Principal {
                kind: PrincipalKind::User,
                can_login: Some(true),
                superuser: Some(yes(r, "super")),
                disabled: None,
                member_of: Vec::new(),
                details: vec![("Tipo".into(), "Usuario".into())],
                system: name == "crate",
                name,
            }
        })
        .collect();
    // Roles came with CrateDB 5.6.
    if let Ok(roles) = s.text("SELECT name FROM sys.roles ORDER BY name").await {
        out.extend(roles.iter().filter_map(|r| cell(r, "name")).map(|name| Principal {
            name,
            kind: PrincipalKind::Role,
            can_login: Some(false),
            details: vec![("Tipo".into(), "Rol".into())],
            ..Default::default()
        }));
    }
    Ok(out)
}

pub async fn grants(s: &PgSession, principal: &str) -> Result<Vec<Grant>> {
    let d = dialect(s.variant).ok_or_else(unsupported)?;
    let members = memberships(s, d).await;
    // grantee -> the direct role it's held through (None: directly).
    let mut via: HashMap<String, Option<String>> = HashMap::new();
    via.insert(principal.to_string(), None);
    for (role, through) in closure(principal, &members) {
        via.insert(role, Some(through));
    }
    // (grantee, grant) as the catalog reports them.
    let found: Vec<(String, Grant)> = match d {
        Dialect::Pg => pg_grants(s, &via).await?,
        Dialect::Cockroach => cockroach_grants(s, principal).await?,
        Dialect::Materialize => materialize_grants(s, principal).await?,
        Dialect::Redshift => redshift_grants(s, &via).await?,
        Dialect::Crate => crate_grants(s, &via).await?,
        Dialect::H2 => h2::grants(s, principal, &via).await?,
        Dialect::RisingWave => risingwave::grants(s, &via).await?,
    };
    let mut out: Vec<Grant> = found
        .into_iter()
        .filter_map(|(grantee, mut g)| {
            g.via = via.get(&grantee)?.clone();
            Some(g)
        })
        .collect();
    out.sort_by(|a, b| {
        (a.via.is_some(), &a.object_kind, &a.object, &a.privilege).cmp(&(b.via.is_some(), &b.object_kind, &b.object, &b.privilege))
    });
    out.dedup();
    Ok(out)
}

/// `'a', 'b'` for an `IN (…)`.
fn name_list(v: Variant, via: &HashMap<String, Option<String>>) -> String {
    via.keys().map(|n| lit(v, n)).collect::<Vec<_>>().join(", ")
}

fn grant_row(r: &tokio_postgres::SimpleQueryRow) -> Option<(String, Grant)> {
    Some((
        cell(r, "grantee")?,
        Grant {
            privilege: cell(r, "privilege")?,
            object: cell(r, "object"),
            object_kind: cell(r, "kind"),
            grantable: yes(r, "grantable"),
            denied: false,
            via: None,
        },
    ))
}

/// The ACLs of the session's database: relations, schemas, the database
/// itself and routines, leaving out what owners hold by owning.
async fn pg_grants(s: &PgSession, via: &HashMap<String, Option<String>>) -> Result<Vec<(String, Grant)>> {
    let filter = s.filter("n.nspname");
    let routine_kind = if s.version >= 110000 {
        "CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END"
    } else {
        "'function'"
    };
    let sql = format!(
        "WITH acl AS (
           SELECT n.nspname || '.' || c.relname AS object,
                  CASE WHEN c.relkind = 'v' THEN 'view' WHEN c.relkind = 'm' THEN 'materialized_view'
                       WHEN c.relkind = 'S' THEN 'sequence' ELSE 'table' END AS kind,
                  c.relowner AS owner, (aclexplode(c.relacl)).*
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm', 'S') AND c.relacl IS NOT NULL AND {filter}
           UNION ALL
           SELECT n.nspname, 'schema', n.nspowner, (aclexplode(n.nspacl)).*
             FROM pg_namespace n WHERE n.nspacl IS NOT NULL AND {filter}
           UNION ALL
           SELECT d.datname, 'database', d.datdba, (aclexplode(d.datacl)).*
             FROM pg_database d WHERE d.datname = current_database() AND d.datacl IS NOT NULL
           UNION ALL
           SELECT n.nspname || '.' || p.proname, {routine_kind}, p.proowner, (aclexplode(p.proacl)).*
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
            WHERE p.proacl IS NOT NULL AND {filter}
         )
         SELECT r.rolname AS grantee, acl.privilege_type AS privilege, acl.is_grantable AS grantable,
                acl.object, acl.kind
           FROM acl JOIN pg_roles r ON r.oid = acl.grantee
          WHERE r.rolname IN ({names})
            AND NOT (acl.grantee = acl.owner AND acl.grantor = acl.owner)",
        names = name_list(s.variant, via),
    );
    Ok(s.text(&sql).await?.iter().filter_map(grant_row).collect())
}

/// `SHOW GRANTS FOR` already follows role membership; PUBLIC's and the
/// system schemas' grants are left out.
async fn cockroach_grants(s: &PgSession, principal: &str) -> Result<Vec<(String, Grant)>> {
    let sql = format!("SHOW GRANTS FOR {}", quote_ident(Quote::Double, principal));
    let rows = s.text(&sql).await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let schema = cell(r, "schema_name");
            if matches!(schema.as_deref(), Some("pg_catalog" | "information_schema" | "crdb_internal" | "pg_extension")) {
                return None;
            }
            let kind = match cell(r, "object_type").as_deref() {
                Some("routine") => "function".to_string(),
                Some(k) => k.to_string(),
                None => "table".to_string(),
            };
            let object = match (kind.as_str(), schema, cell(r, "object_name")) {
                ("database", _, _) => cell(r, "database_name"),
                (_, Some(sc), Some(o)) => Some(format!("{sc}.{o}")),
                (_, sc, o) => o.or(sc),
            };
            Some((
                cell(r, "grantee")?,
                Grant {
                    privilege: cell(r, "privilege_type")?,
                    object,
                    object_kind: Some(kind),
                    grantable: yes(r, "is_grantable"),
                    denied: false,
                    via: None,
                },
            ))
        })
        .collect())
}

/// `SHOW PRIVILEGES FOR` follows role membership too; it covers every
/// database, so other databases' objects and PUBLIC's grants are left out.
async fn materialize_grants(s: &PgSession, principal: &str) -> Result<Vec<(String, Grant)>> {
    let sql = format!("SHOW PRIVILEGES FOR {}", quote_ident(Quote::Double, principal));
    let rows = s.text(&sql).await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            if cell(r, "database").is_some_and(|db| db != s.database) {
                return None;
            }
            let kind = cell(r, "object_type").unwrap_or_default().replace('-', "_");
            if kind == "database" && cell(r, "name").as_deref() != Some(s.database.as_str()) {
                return None;
            }
            let object = match (kind.as_str(), cell(r, "schema"), cell(r, "name")) {
                ("system", _, _) => None,
                (_, Some(sc), Some(n)) => Some(format!("{sc}.{n}")),
                (_, sc, n) => n.or(sc),
            };
            Some((
                cell(r, "grantee")?,
                Grant {
                    privilege: cell(r, "privilege_type")?,
                    object,
                    object_kind: Some(kind),
                    grantable: false,
                    denied: false,
                    via: None,
                },
            ))
        })
        .collect())
}

/// Redshift's `svv_*_privileges` views (2023 on) name the grantee and
/// whether it's a user, a group or a role.
async fn redshift_grants(s: &PgSession, via: &HashMap<String, Option<String>>) -> Result<Vec<(String, Grant)>> {
    let names = name_list(s.variant, via);
    let grantee = "CASE identity_type WHEN 'group' THEN 'GROUP ' || identity_name ELSE identity_name END";
    let queries = [
        format!(
            "SELECT {grantee} AS grantee, privilege_type AS privilege, admin_option AS grantable,
                    namespace_name || '.' || relation_name AS object, 'table' AS kind
               FROM svv_relation_privileges"
        ),
        format!(
            "SELECT {grantee} AS grantee, privilege_type AS privilege, admin_option AS grantable,
                    namespace_name AS object, 'schema' AS kind
               FROM svv_schema_privileges"
        ),
        format!(
            "SELECT {grantee} AS grantee, privilege_type AS privilege, admin_option AS grantable,
                    database_name AS object, 'database' AS kind
               FROM svv_database_privileges WHERE database_name = current_database()"
        ),
        format!(
            "SELECT {grantee} AS grantee, privilege_type AS privilege, admin_option AS grantable,
                    namespace_name || '.' || function_name AS object, 'function' AS kind
               FROM svv_function_privileges"
        ),
    ];
    let mut out = Vec::new();
    let mut last = None;
    for q in queries {
        match s.text(&format!("SELECT * FROM ({q}) g WHERE grantee IN ({names})")).await {
            Ok(rows) => out.extend(rows.iter().filter_map(grant_row)),
            Err(e) => {
                tracing::debug!("redshift privileges view unavailable: {e}");
                last = Some(e);
            }
        }
    }
    match last {
        Some(e) if out.is_empty() => Err(e),
        _ => Ok(out),
    }
}

async fn crate_grants(s: &PgSession, via: &HashMap<String, Option<String>>) -> Result<Vec<(String, Grant)>> {
    let sql = format!(
        "SELECT grantee, type AS privilege, class, ident, state FROM sys.privileges WHERE grantee IN ({})",
        name_list(s.variant, via)
    );
    Ok(s.text(&sql)
        .await?
        .iter()
        .filter_map(|r| {
            let class = cell(r, "class").unwrap_or_default().to_lowercase();
            Some((
                cell(r, "grantee")?,
                Grant {
                    privilege: cell(r, "privilege")?,
                    object: cell(r, "ident").filter(|_| class != "cluster"),
                    object_kind: (class != "cluster").then_some(class),
                    grantable: false,
                    denied: cell(r, "state").as_deref() == Some("DENY"),
                    via: None,
                },
            ))
        })
        .collect())
}

// -- scripts ---------------------------------------------------------------

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

/// Privilege names come from `spec()` or the user: letters and spaces only.
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let x = x.split_whitespace().collect::<Vec<_>>().join(" ");
        if x.is_empty() || !x.chars().all(|c| c.is_ascii_alphabetic() || c == ' ') {
            return Err(Error::Query(format!("«{x}» no es un nombre de permiso válido")));
        }
        out.push(x.to_uppercase());
    }
    Ok(out.join(", "))
}

/// A routine's argument list as the catalog prints it (`f1(integer, text)`):
/// type names only, nothing that could end the statement.
fn routine_name(name: &str) -> Result<String> {
    match name.split_once('(') {
        Some((n, rest)) => {
            let args = rest.strip_suffix(')').ok_or_else(|| Error::Query(format!("«{name}» no es un nombre de rutina válido")))?;
            if !args.chars().all(|c| c.is_alphanumeric() || " ,._[]\"".contains(c)) {
                return Err(Error::Query(format!("«{name}» no es un nombre de rutina válido")));
            }
            Ok(format!("{}({args})", q(n)))
        }
        None => Ok(q(name)),
    }
}

/// `TABLE "s"."t"`, `SCHEMA "s"`, `FUNCTION "s"."f"`…
fn target(o: &ObjectRef) -> Result<String> {
    let qualified = |name: String| match o.schema() {
        Some(sc) => format!("{}.{name}", q(sc)),
        None => name,
    };
    Ok(match o.kind.as_str() {
        "schema" => format!("SCHEMA {}", q(&o.name)),
        "database" => format!("DATABASE {}", q(&o.name)),
        "sequence" => format!("SEQUENCE {}", qualified(q(&o.name))),
        "type" => format!("TYPE {}", qualified(q(&o.name))),
        "cluster" => format!("CLUSTER {}", q(&o.name)),
        "connection" => format!("CONNECTION {}", qualified(q(&o.name))),
        "secret" => format!("SECRET {}", qualified(q(&o.name))),
        "function" => format!("FUNCTION {}", qualified(routine_name(&o.name)?)),
        "procedure" => format!("PROCEDURE {}", qualified(routine_name(&o.name)?)),
        _ => format!("TABLE {}", qualified_name(Quote::Double, o.schema(), &o.name)),
    })
}

/// `statement` (with `%I` for the database's name and the grantee's as
/// `%I`) run on the current database: `GRANT … ON DATABASE` needs a name,
/// and the script is written before knowing which database it runs on.
fn on_current_database(statement: &str, grantee: &str) -> String {
    let arg = lit(Variant::Postgres, grantee);
    let mut tag = "$dbine$".to_string();
    let mut n = 0;
    while arg.contains(&tag[..tag.len() - 1]) {
        n += 1;
        tag = format!("$dbine{n}$");
    }
    format!(
        "-- Sobre la base en la que se ejecuta (current_database()).\n\
         DO {tag} BEGIN EXECUTE format('{statement}', current_database(), {arg}); END {tag};"
    )
}

pub fn script(v: Variant, a: &SecurityAction) -> Result<String> {
    match dialect(v).ok_or_else(unsupported)? {
        Dialect::Pg => pg_script(v, a, true),
        Dialect::Cockroach | Dialect::Materialize => pg_script(v, a, false),
        Dialect::Redshift => redshift_script(a),
        Dialect::Crate => crate_script(v, a),
        Dialect::H2 => h2::script(a),
        Dialect::RisingWave => risingwave::script(a),
    }
}

fn cockroach_database() -> Error {
    Error::Query("en CockroachDB los permisos sobre la base llevan su nombre: escribí GRANT … ON DATABASE <base> en una consulta".into())
}

fn no_grant_option() -> Error {
    Error::Query("Materialize no permite que un rol otorgue a otros sus permisos (no hay WITH GRANT OPTION)".into())
}

/// PostgreSQL's and CockroachDB's (`cascade`: `REVOKE … CASCADE`, which
/// CockroachDB doesn't take).
fn pg_script(v: Variant, a: &SecurityAction, cascade: bool) -> Result<String> {
    // openGauss asks every role for a password, or for none explicitly.
    let no_password = if v == Variant::OpenGauss { " PASSWORD DISABLE" } else { "" };
    let pw = |p: &Option<String>| {
        p.as_deref().filter(|p| !p.is_empty()).map_or(no_password.to_string(), |p| format!(" PASSWORD {}", lit(v, p)))
    };
    Ok(match a {
        SecurityAction::CreateUser { name, password } => format!("CREATE ROLE {} WITH LOGIN{};", q(name), pw(password)),
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {}{no_password};", q(name)),
        SecurityAction::Drop { name, .. } => format!(
            "-- Si tiene objetos o permisos en alguna base, antes, en cada una:\n\
             -- REASSIGN OWNED BY {n} TO CURRENT_USER;\n\
             -- DROP OWNED BY {n};\n\
             DROP ROLE {n};",
            n = q(name)
        ),
        SecurityAction::SetPassword { name, password } => format!("ALTER ROLE {} WITH PASSWORD {};", q(name), lit(v, password)),
        SecurityAction::SetLogin { name, enabled } => {
            format!("ALTER ROLE {} WITH {};", q(name), if *enabled { "LOGIN" } else { "NOLOGIN" })
        }
        SecurityAction::Grant { grantable: true, .. } if v == Variant::Materialize => return Err(no_grant_option()),
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            let option = if *grantable { " WITH GRANT OPTION" } else { "" };
            match object {
                Some(o) => format!("GRANT {} ON {} TO {}{option};", privileges(p)?, target(o)?, q(to)),
                None if v == Variant::Materialize => format!("GRANT {} ON SYSTEM TO {};", privileges(p)?, q(to)),
                None if v == Variant::Cockroach => return Err(cockroach_database()),
                None => on_current_database(&format!("GRANT {} ON DATABASE %I TO %I{option}", privileges(p)?), to),
            }
        }
        SecurityAction::Revoke { privileges: p, object, from } => {
            let cascade = if cascade { " CASCADE" } else { "" };
            match object {
                Some(o) => format!("REVOKE {} ON {} FROM {}{cascade};", privileges(p)?, target(o)?, q(from)),
                None if v == Variant::Materialize => format!("REVOKE {} ON SYSTEM FROM {};", privileges(p)?, q(from)),
                None if v == Variant::Cockroach => return Err(cockroach_database()),
                None => on_current_database(&format!("REVOKE {} ON DATABASE %I FROM %I{cascade}", privileges(p)?), from),
            }
        }
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", q(role), q(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", q(role), q(member)),
    })
}

/// A Redshift principal as it's written: `"ana"` or `GROUP "ventas"`.
fn rs_grantee(name: &str) -> String {
    match name.strip_prefix("GROUP ") {
        Some(g) => format!("GROUP {}", q(g)),
        None => q(name),
    }
}
fn rs_group(name: &str) -> String {
    q(name.strip_prefix("GROUP ").unwrap_or(name))
}

fn redshift_script(a: &SecurityAction) -> Result<String> {
    let v = Variant::Redshift;
    let on = |object: &Option<ObjectRef>| -> Result<String> {
        match object {
            Some(o) if matches!(o.kind.as_str(), "function" | "procedure") => Err(Error::Query(
                "en Redshift los permisos sobre funciones y procedimientos llevan los tipos de sus argumentos: escribilo en una consulta".into(),
            )),
            Some(o) => target(o),
            None => Err(Error::Query(
                "en Redshift los permisos sobre la base llevan su nombre: escribí GRANT … ON DATABASE <base> en una consulta".into(),
            )),
        }
    };
    Ok(match a {
        SecurityAction::CreateUser { name, password } => match password.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("CREATE USER {} PASSWORD {};", q(name), lit(v, p)),
            None => format!("CREATE USER {} PASSWORD DISABLE;", q(name)),
        },
        SecurityAction::CreateRole { name } => format!("CREATE GROUP {};", rs_group(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP GROUP {};", rs_group(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} PASSWORD {};", q(name), lit(v, password)),
        SecurityAction::SetLogin { name, enabled: false } => format!("ALTER USER {} PASSWORD DISABLE;", q(name)),
        SecurityAction::SetLogin { enabled: true, .. } => {
            return Err(Error::Query("en Redshift el ingreso se habilita asignando una contraseña nueva".into()))
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => format!(
            "GRANT {} ON {} TO {}{};",
            privileges(p)?,
            on(object)?,
            rs_grantee(to),
            if *grantable { " WITH GRANT OPTION" } else { "" }
        ),
        SecurityAction::Revoke { privileges: p, object, from } => {
            format!("REVOKE {} ON {} FROM {};", privileges(p)?, on(object)?, rs_grantee(from))
        }
        SecurityAction::AddMember { role, member } => format!("ALTER GROUP {} ADD USER {};", rs_group(role), q(member)),
        SecurityAction::RemoveMember { role, member } => format!("ALTER GROUP {} DROP USER {};", rs_group(role), q(member)),
    })
}

fn crate_script(v: Variant, a: &SecurityAction) -> Result<String> {
    let on = |object: &Option<ObjectRef>| -> String {
        match object {
            None => String::new(),
            Some(o) => match o.kind.as_str() {
                "schema" => format!(" ON SCHEMA {}", q(&o.name)),
                "view" => format!(" ON VIEW {}", qualified_name(Quote::Double, o.schema(), &o.name)),
                _ => format!(" ON TABLE {}", qualified_name(Quote::Double, o.schema(), &o.name)),
            },
        }
    };
    Ok(match a {
        SecurityAction::CreateUser { name, password } => match password.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("CREATE USER {} WITH (password = {});", q(name), lit(v, p)),
            None => format!("CREATE USER {};", q(name)),
        },
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(name)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} SET (password = {});", q(name), lit(v, password)),
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported("CrateDB no deshabilita usuarios: se les puede cambiar la contraseña o borrarlos".into()))
        }
        SecurityAction::Grant { grantable: true, .. } => {
            return Err(Error::Query("CrateDB no permite que un usuario otorgue a otros sus permisos (no hay WITH GRANT OPTION)".into()))
        }
        SecurityAction::Grant { privileges: p, object, to, .. } => format!("GRANT {}{} TO {};", privileges(p)?, on(object), q(to)),
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {}{} FROM {};", privileges(p)?, on(object), q(from)),
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", q(role), q(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", q(role), q(member)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Option<ObjectRef> {
        Some(ObjectRef { kind: "table".into(), schema: Some("pub\"lic".into()), name: "fac\"turas".into() })
    }

    #[test]
    fn postgres_scripts_quote_and_escape() {
        let s = |a| script(Variant::Postgres, &a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana\"x".into(), password: Some("p'w\\".into()) }),
            "CREATE ROLE \"ana\"\"x\" WITH LOGIN PASSWORD 'p''w\\';"
        );
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: None }), "CREATE ROLE \"ana\" WITH LOGIN;");
        assert_eq!(s(SecurityAction::CreateRole { name: "lectores".into() }), "CREATE ROLE \"lectores\";");
        assert!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }).ends_with("\nDROP ROLE \"ana\";"));
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "x'y".into() }), "ALTER ROLE \"ana\" WITH PASSWORD 'x''y';");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ALTER ROLE \"ana\" WITH NOLOGIN;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), " update ".into()], object: table(), to: "ana".into(), grantable: true }),
            "GRANT SELECT, UPDATE ON TABLE \"pub\"\"lic\".\"fac\"\"turas\" TO \"ana\" WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Revoke {
                privileges: vec!["usage".into()],
                object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "ventas".into() }),
                from: "r1".into()
            }),
            "REVOKE USAGE ON SCHEMA \"ventas\" FROM \"r1\" CASCADE;"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["EXECUTE".into()],
                object: Some(ObjectRef { kind: "function".into(), schema: Some("public".into()), name: "f(integer, text[])".into() }),
                to: "ana".into(),
                grantable: false
            }),
            "GRANT EXECUTE ON FUNCTION \"public\".\"f\"(integer, text[]) TO \"ana\";"
        );
        assert_eq!(s(SecurityAction::AddMember { role: "lectores".into(), member: "ana".into() }), "GRANT \"lectores\" TO \"ana\";");
        assert_eq!(s(SecurityAction::RemoveMember { role: "lectores".into(), member: "ana".into() }), "REVOKE \"lectores\" FROM \"ana\";");
    }

    #[test]
    fn database_grants_run_on_the_current_database() {
        let s = script(
            Variant::Postgres,
            &SecurityAction::Grant { privileges: vec!["CONNECT".into(), "temporary".into()], object: None, to: "o'k".into(), grantable: false },
        )
        .unwrap();
        assert!(s.ends_with(
            "DO $dbine$ BEGIN EXECUTE format('GRANT CONNECT, TEMPORARY ON DATABASE %I TO %I', current_database(), 'o''k'); END $dbine$;"
        ));
        // A name that holds the dollar-quote tag gets another tag.
        let s = script(Variant::Postgres, &SecurityAction::Revoke { privileges: vec!["CONNECT".into()], object: None, from: "$dbine$".into() }).unwrap();
        assert!(s.contains("DO $dbine1$ BEGIN"), "{s}");
    }

    #[test]
    fn privilege_and_routine_names_cannot_inject() {
        for bad in ["SELECT; DROP TABLE x", "SELECT --", "SEL'ECT", "", "CONNECT)"] {
            let a = SecurityAction::Grant { privileges: vec![bad.into()], object: table(), to: "a".into(), grantable: false };
            assert!(script(Variant::Postgres, &a).is_err(), "{bad}");
            assert!(script(Variant::Redshift, &a).is_err(), "{bad}");
        }
        assert!(script(Variant::Postgres, &SecurityAction::Grant { privileges: vec![], object: None, to: "a".into(), grantable: false }).is_err());
        let f = |name: &str| SecurityAction::Grant {
            privileges: vec!["EXECUTE".into()],
            object: Some(ObjectRef { kind: "function".into(), schema: None, name: name.into() }),
            to: "a".into(),
            grantable: false,
        };
        assert!(script(Variant::Postgres, &f("f(int); DROP TABLE t; --)")).is_err());
        assert!(script(Variant::Postgres, &f("f(int")).is_err());
    }

    #[test]
    fn opengauss_roles_say_they_have_no_password() {
        let s = |a| script(Variant::OpenGauss, &a).unwrap();
        assert_eq!(s(SecurityAction::CreateRole { name: "r".into() }), "CREATE ROLE \"r\" PASSWORD DISABLE;");
        assert_eq!(s(SecurityAction::CreateUser { name: "u".into(), password: None }), "CREATE ROLE \"u\" WITH LOGIN PASSWORD DISABLE;");
        assert_eq!(s(SecurityAction::CreateUser { name: "u".into(), password: Some("x".into()) }), "CREATE ROLE \"u\" WITH LOGIN PASSWORD 'x';");
    }

    #[test]
    fn cockroach_revokes_without_cascade() {
        let a = SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: table(), from: "ana".into() };
        assert_eq!(script(Variant::Cockroach, &a).unwrap(), "REVOKE SELECT ON TABLE \"pub\"\"lic\".\"fac\"\"turas\" FROM \"ana\";");
        let a = SecurityAction::Revoke { privileges: vec!["CONNECT".into()], object: None, from: "ana".into() };
        assert!(script(Variant::Cockroach, &a).is_err());
        assert!(!spec(Variant::Cockroach).unwrap().object_kinds.contains(&""));
    }

    #[test]
    fn materialize_grants_system_privileges() {
        let s = |a| script(Variant::Materialize, &a).unwrap();
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["createrole".into()], object: None, to: "ana".into(), grantable: false }),
            "GRANT CREATEROLE ON SYSTEM TO \"ana\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: table(), from: "ana".into() }),
            "REVOKE SELECT ON TABLE \"pub\"\"lic\".\"fac\"\"turas\" FROM \"ana\";"
        );
        assert!(script(Variant::Materialize, &SecurityAction::Grant { privileges: vec!["SELECT".into()], object: table(), to: "a".into(), grantable: true }).is_err());
    }

    #[test]
    fn redshift_has_users_and_groups() {
        let s = |a| script(Variant::Redshift, &a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("P\\w'1".into()) }), "CREATE USER \"ana\" PASSWORD 'P\\\\w''1';");
        assert_eq!(s(SecurityAction::CreateRole { name: "ventas".into() }), "CREATE GROUP \"ventas\";");
        assert_eq!(s(SecurityAction::Drop { name: "GROUP ventas".into(), kind: PrincipalKind::Role }), "DROP GROUP \"ventas\";");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: table(), to: "GROUP ventas".into(), grantable: false }),
            "GRANT SELECT ON TABLE \"pub\"\"lic\".\"fac\"\"turas\" TO GROUP \"ventas\";"
        );
        assert_eq!(s(SecurityAction::AddMember { role: "GROUP ventas".into(), member: "ana".into() }), "ALTER GROUP \"ventas\" ADD USER \"ana\";");
        assert!(script(Variant::Redshift, &SecurityAction::Grant { privileges: vec!["CREATE".into()], object: None, to: "a".into(), grantable: false }).is_err());
    }

    #[test]
    fn cratedb_scripts() {
        let s = |a| script(Variant::CrateDb, &a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("x'y".into()) }), "CREATE USER \"ana\" WITH (password = 'x''y');");
        assert_eq!(s(SecurityAction::Grant { privileges: vec!["dql".into()], object: None, to: "ana".into(), grantable: false }), "GRANT DQL TO \"ana\";");
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["DML".into()], object: table(), from: "ana".into() }),
            "REVOKE DML ON TABLE \"pub\"\"lic\".\"fac\"\"turas\" FROM \"ana\";"
        );
        assert!(script(Variant::CrateDb, &SecurityAction::SetLogin { name: "ana".into(), enabled: false }).is_err());
    }

    #[test]
    fn specs_follow_the_engine() {
        assert!(spec(Variant::Denodo).is_none());
        assert!(script(Variant::Denodo, &SecurityAction::CreateRole { name: "r".into() }).is_err());
        let pg = spec(Variant::Postgres).unwrap();
        assert!(!pg.per_database);
        assert!(pg.object_kinds.contains(&"") && pg.object_kinds.contains(&"materialized_view"));
        // Yellowbrick's explorer has no procedures or materialized views.
        let yb = spec(Variant::Yellowbrick).unwrap();
        assert!(!yb.object_kinds.contains(&"procedure") && !yb.object_kinds.contains(&"materialized_view"));
    }

    #[test]
    fn membership_closure_names_the_direct_role() {
        let m = |a: &str, b: &str| (a.to_string(), b.to_string());
        let members = [m("ana", "lectores"), m("lectores", "base"), m("base", "lectores"), m("ana", "otros")];
        let mut c = closure("ana", &members);
        c.sort();
        assert_eq!(c, vec![m("base", "lectores"), m("lectores", "lectores"), m("otros", "otros")]);
    }
}

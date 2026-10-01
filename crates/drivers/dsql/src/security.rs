//! Users, roles and permissions (docs/usuarios-y-permisos.md) for Aurora
//! DSQL: PostgreSQL roles, signed in through IAM. A role with `LOGIN` is a
//! user; the IAM roles that may sign in as it are its mappings
//! (`AWS IAM GRANT <role> TO '<arn>'`, listed in `sys.iam_pg_role_mappings`).
//! There are no passwords. Privileges are read from the catalog's ACLs
//! (`aclexplode`) and, if the cluster refuses that, from
//! `information_schema.table_privileges`. The only database is `postgres`,
//! so database-wide grants name it directly.

use crate::{err, DATABASE, SYSTEM_SCHEMAS};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SchemaSpec, SecurityAction, SecuritySpec};
use std::collections::HashMap;
use tokio_postgres::{Client, SimpleQueryMessage, SimpleQueryRow};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "USAGE", "EXECUTE", "CREATE", "CONNECT", "ALL PRIVILEGES"],
        object_kinds: vec!["", "schema", "table", "view", "sequence", "function"],
        create_user: true,
        create_role: true,
        // IAM authentication: DSQL roles have no password.
        passwords: false,
        membership: true,
        per_database: false,
    }
}

// -- reading ---------------------------------------------------------------

async fn rows(c: &Client, sql: &str) -> Result<Vec<SimpleQueryRow>> {
    Ok(c.simple_query(sql)
        .await
        .map_err(err)?
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(r),
            _ => None,
        })
        .collect())
}

fn cell(r: &SimpleQueryRow, name: &str) -> Option<String> {
    r.try_get(name).ok().flatten().map(str::to_string)
}

fn yes(r: &SimpleQueryRow, name: &str) -> bool {
    matches!(cell(r, name).as_deref(), Some("t" | "true"))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// (member, role) pairs.
async fn memberships(c: &Client) -> Vec<(String, String)> {
    let sql = "SELECT m.rolname AS member, r.rolname AS role
                 FROM pg_auth_members am
                 JOIN pg_roles r ON r.oid = am.roleid
                 JOIN pg_roles m ON m.oid = am.member";
    match rows(c, sql).await {
        Ok(r) => r.iter().filter_map(|r| Some((cell(r, "member")?, cell(r, "role")?))).collect(),
        Err(e) => {
            tracing::debug!("dsql: role memberships unavailable: {e}");
            Vec::new()
        }
    }
}

/// (database role, IAM ARN) pairs; none outside DSQL.
async fn iam_mappings(c: &Client) -> Vec<(String, String)> {
    match rows(c, "SELECT pg_role_name AS role, arn FROM sys.iam_pg_role_mappings ORDER BY arn").await {
        Ok(r) => r.iter().filter_map(|r| Some((cell(r, "role")?, cell(r, "arn")?))).collect(),
        Err(e) => {
            tracing::debug!("dsql: IAM role mappings unavailable: {e}");
            Vec::new()
        }
    }
}

fn is_system(name: &str, oid: u64) -> bool {
    oid < 16384 || name == "admin" || name.starts_with("pg_") || name.starts_with("sys_") || name.starts_with("rds")
}

pub async fn principals(c: &Client) -> Result<Vec<Principal>> {
    let list = rows(
        c,
        "SELECT r.oid::text AS oid, r.rolname AS name, r.rolcanlogin AS login, r.rolsuper AS super,
                r.rolcreaterole AS createrole, r.rolcreatedb AS createdb, r.rolconnlimit::text AS connlimit
           FROM pg_roles r ORDER BY r.rolname",
    )
    .await?;
    let iam = iam_mappings(c).await;
    let members = memberships(c).await;
    Ok(list
        .iter()
        .map(|r| {
            let name = cell(r, "name").unwrap_or_default();
            let login = yes(r, "login");
            let arns: Vec<&str> = iam.iter().filter(|(role, _)| *role == name).map(|(_, a)| a.as_str()).collect();
            // A role IAM can sign in as, even if its login is off, is a
            // (disabled) user.
            let user = login || !arns.is_empty();
            let oid: u64 = cell(r, "oid").and_then(|o| o.parse().ok()).unwrap_or(u64::MAX);
            let mut details = vec![(
                "Tipo".to_string(),
                if user { "Usuario (rol con ingreso por IAM)" } else { "Rol (sin ingreso)" }.to_string(),
            )];
            if name == "admin" {
                details.push(("Ingreso".into(), "token DbConnectAdmin".into()));
            } else if user {
                details.push(("Ingreso".into(), "token DbConnect".into()));
            }
            if !arns.is_empty() {
                details.push(("Roles de IAM".into(), arns.join("\n")));
            } else if login && name != "admin" {
                details.push(("Roles de IAM".into(), "ninguno asociado: nadie puede ingresar con este rol".into()));
            }
            if yes(r, "createrole") {
                details.push(("Puede crear roles".into(), "sí".into()));
            }
            if let Some(n) = cell(r, "connlimit").filter(|n| n != "-1") {
                details.push(("Límite de conexiones".into(), n));
            }
            let mut member_of: Vec<String> = members.iter().filter(|(m, _)| *m == name).map(|(_, r)| r.clone()).collect();
            member_of.sort();
            Principal {
                kind: if user { PrincipalKind::User } else { PrincipalKind::Role },
                can_login: Some(login),
                superuser: Some(yes(r, "super") || name == "admin"),
                disabled: user.then_some(!login),
                member_of,
                details,
                system: is_system(&name, oid),
                name,
            }
        })
        .collect())
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

fn grant_row(r: &SimpleQueryRow) -> Option<(String, Grant)> {
    Some((
        cell(r, "grantee")?,
        Grant {
            privilege: cell(r, "privilege")?,
            object: cell(r, "object"),
            object_kind: cell(r, "kind"),
            grantable: matches!(cell(r, "grantable").as_deref(), Some("t" | "true" | "YES")),
            denied: false,
            via: None,
        },
    ))
}

pub async fn grants(c: &Client, principal: &str) -> Result<Vec<Grant>> {
    let members = memberships(c).await;
    let mut via: HashMap<String, Option<String>> = HashMap::new();
    via.insert(principal.to_string(), None);
    for (role, through) in closure(principal, &members) {
        via.insert(role, Some(through));
    }
    let names = via.keys().map(|n| lit(n)).collect::<Vec<_>>().join(", ");
    // Relations, schemas, the database and functions, leaving out what
    // owners hold by owning.
    let acl = format!(
        "WITH acl AS (
           SELECT n.nspname || '.' || c.relname AS object,
                  CASE WHEN c.relkind = 'v' THEN 'view' WHEN c.relkind = 'S' THEN 'sequence' ELSE 'table' END AS kind,
                  c.relowner AS owner, (aclexplode(c.relacl)).*
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S') AND c.relacl IS NOT NULL AND n.nspname NOT IN {SYSTEM_SCHEMAS}
           UNION ALL
           SELECT n.nspname, 'schema', n.nspowner, (aclexplode(n.nspacl)).*
             FROM pg_namespace n WHERE n.nspacl IS NOT NULL AND n.nspname NOT IN {SYSTEM_SCHEMAS}
           UNION ALL
           SELECT d.datname, 'database', d.datdba, (aclexplode(d.datacl)).*
             FROM pg_database d WHERE d.datname = current_database() AND d.datacl IS NOT NULL
           UNION ALL
           SELECT n.nspname || '.' || p.proname, 'function', p.proowner, (aclexplode(p.proacl)).*
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
            WHERE p.proacl IS NOT NULL AND n.nspname NOT IN {SYSTEM_SCHEMAS}
         )
         SELECT r.rolname AS grantee, acl.privilege_type AS privilege, acl.is_grantable::text AS grantable,
                acl.object, acl.kind
           FROM acl JOIN pg_roles r ON r.oid = acl.grantee
          WHERE r.rolname IN ({names})
            AND NOT (acl.grantee = acl.owner AND acl.grantor = acl.owner)"
    );
    let found = match rows(c, &acl).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("dsql: ACL catalog unavailable ({e}); using information_schema");
            let sql = format!(
                "SELECT grantee, privilege_type AS privilege, is_grantable AS grantable,
                        table_schema || '.' || table_name AS object, 'table' AS kind
                   FROM information_schema.table_privileges
                  WHERE grantee IN ({names}) AND table_schema NOT IN {SYSTEM_SCHEMAS}"
            );
            rows(c, &sql).await?
        }
    };
    let mut out: Vec<Grant> = found
        .iter()
        .filter_map(grant_row)
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

/// A function, optionally with its argument types (`f(integer, text)`):
/// type names only, nothing that could end the statement.
fn routine_name(name: &str) -> Result<String> {
    match name.split_once('(') {
        Some((n, rest)) => {
            let args = rest.strip_suffix(')').ok_or_else(|| Error::Query(format!("«{name}» no es un nombre de función válido")))?;
            if !args.chars().all(|c| c.is_alphanumeric() || " ,._[]\"".contains(c)) {
                return Err(Error::Query(format!("«{name}» no es un nombre de función válido")));
            }
            Ok(format!("{}({args})", q(n)))
        }
        None => Ok(q(name)),
    }
}

/// What a privilege applies to: the database, a schema or an object.
fn target(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else { return Ok(format!("DATABASE {}", q(DATABASE))) };
    let qualified = |name: String| match o.schema() {
        Some(sc) => format!("{}.{name}", q(sc)),
        None => name,
    };
    Ok(match o.kind.as_str() {
        "database" => format!("DATABASE {}", q(&o.name)),
        "schema" => format!("SCHEMA {}", q(&o.name)),
        "sequence" => format!("SEQUENCE {}", qualified(q(&o.name))),
        "function" => format!("FUNCTION {}", qualified(routine_name(&o.name)?)),
        _ => format!("TABLE {}", qualified_name(Quote::Double, o.schema(), &o.name)),
    })
}

fn no_passwords() -> Error {
    Error::Unsupported("Aurora DSQL no usa contraseñas: se ingresa con un token de IAM (AWS IAM GRANT <rol> TO '<arn>')".into())
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { password: Some(p), .. } if !p.is_empty() => return Err(no_passwords()),
        SecurityAction::CreateUser { name, .. } => format!(
            "CREATE ROLE {n} WITH LOGIN;\n\
             -- Para ingresar hace falta asociarle un rol de IAM (con permiso dsql:DbConnect):\n\
             -- AWS IAM GRANT {n} TO 'arn:aws:iam::<cuenta>:role/<rol-de-iam>';",
            n = q(name)
        ),
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", q(name)),
        SecurityAction::Drop { name, .. } => format!(
            "-- Si tiene roles de IAM asociados (sys.iam_pg_role_mappings), antes:\n\
             -- AWS IAM REVOKE {n} FROM '<arn>';\n\
             DROP ROLE {n};",
            n = q(name)
        ),
        SecurityAction::SetPassword { .. } => return Err(no_passwords()),
        SecurityAction::SetLogin { name, enabled } => {
            format!("ALTER ROLE {} WITH {};", q(name), if *enabled { "LOGIN" } else { "NOLOGIN" })
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => format!(
            "GRANT {} ON {} TO {}{};",
            privileges(p)?,
            target(object)?,
            q(to),
            if *grantable { " WITH GRANT OPTION" } else { "" }
        ),
        SecurityAction::Revoke { privileges: p, object, from } => {
            format!("REVOKE {} ON {} FROM {};", privileges(p)?, target(object)?, q(from))
        }
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", q(role), q(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", q(role), q(member)),
    })
}

// -- schemas -----------------------------------------------------------------

/// "Nuevo esquema…": the schema, grants on it, then `ALTER SCHEMA … OWNER
/// TO` (the creator, not a superuser, couldn't grant once it's someone
/// else's; handing it over needs the creator to be able to act as the new
/// owner, as in PostgreSQL). Dropping offers no CASCADE: DSQL runs one DDL
/// statement per transaction and takes no DDL that drops other objects
/// with it, so a schema is dropped once it's empty.
pub fn schema_spec() -> SchemaSpec {
    SchemaSpec { owner: true, owner_kinds: dbine_driver::SchemaOwnerKinds::Both, cascade: false, privileges: vec!["USAGE", "CREATE", "ALL PRIVILEGES"], grant_option: true }
}

fn schema_name(name: &str) -> Result<String> {
    match name.trim() {
        "" => Err(Error::Query("escribí el nombre del esquema".into())),
        n => Ok(q(n)),
    }
}

/// Owned by the creator; `schema_owner` hands it over afterwards.
pub fn create_schema(name: &str) -> Result<String> {
    Ok(format!("CREATE SCHEMA {};", schema_name(name)?))
}

pub fn schema_owner(name: &str, owner: &str) -> Result<String> {
    match owner.trim() {
        "" => Err(Error::Query("elegí el dueño del esquema".into())),
        o => Ok(format!("ALTER SCHEMA {} OWNER TO {};", schema_name(name)?, q(o))),
    }
}

pub fn drop_schema(name: &str) -> Result<String> {
    Ok(format!("DROP SCHEMA {};", schema_name(name)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana\"b".into(), password: None }),
            "CREATE ROLE \"ana\"\"b\" WITH LOGIN;\n\
             -- Para ingresar hace falta asociarle un rol de IAM (con permiso dsql:DbConnect):\n\
             -- AWS IAM GRANT \"ana\"\"b\" TO 'arn:aws:iam::<cuenta>:role/<rol-de-iam>';"
        );
        assert!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some(String::new()) }).starts_with("CREATE ROLE \"ana\" WITH LOGIN;"));
        assert!(matches!(script(&SecurityAction::CreateUser { name: "a".into(), password: Some("x".into()) }), Err(Error::Unsupported(_))));
        assert!(matches!(script(&SecurityAction::SetPassword { name: "a".into(), password: "x".into() }), Err(Error::Unsupported(_))));
        assert_eq!(s(SecurityAction::CreateRole { name: "lectores".into() }), "CREATE ROLE \"lectores\";");
        assert!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }).ends_with("\nDROP ROLE \"ana\";"));
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ALTER ROLE \"ana\" WITH NOLOGIN;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "UPDATE".into()], object: obj("table", Some("ventas"), "fac\"t"), to: "ana".into(), grantable: true }),
            "GRANT SELECT, UPDATE ON TABLE \"ventas\".\"fac\"\"t\" TO \"ana\" WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["CONNECT".into()], object: None, to: "ana".into(), grantable: false }),
            "GRANT CONNECT ON DATABASE \"postgres\" TO \"ana\";"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["USAGE".into()], object: obj("schema", None, "ventas"), to: "r".into(), grantable: false }),
            "GRANT USAGE ON SCHEMA \"ventas\" TO \"r\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["EXECUTE".into()], object: obj("function", Some("s"), "f(integer, text)"), from: "r".into() }),
            "REVOKE EXECUTE ON FUNCTION \"s\".\"f\"(integer, text) FROM \"r\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["USAGE".into()], object: obj("sequence", Some("s"), "sq"), from: "r".into() }),
            "REVOKE USAGE ON SEQUENCE \"s\".\"sq\" FROM \"r\";"
        );
        assert_eq!(s(SecurityAction::AddMember { role: "lectores".into(), member: "ana".into() }), "GRANT \"lectores\" TO \"ana\";");
        assert_eq!(s(SecurityAction::RemoveMember { role: "lectores".into(), member: "ana".into() }), "REVOKE \"lectores\" FROM \"ana\";");
    }

    #[test]
    fn names_cannot_inject() {
        let g = |p: &str, name: &str| SecurityAction::Grant {
            privileges: vec![p.into()],
            object: obj("function", Some("s"), name),
            to: "r".into(),
            grantable: false,
        };
        assert!(script(&g("SELECT; DROP TABLE x", "f")).is_err());
        assert!(script(&g("EXECUTE", "f(int); DROP TABLE x; --)")).is_err());
        assert!(script(&g("EXECUTE", "f(int")).is_err());
    }

    #[test]
    fn schema_scripts() {
        assert_eq!(create_schema(" ventas ").unwrap(), "CREATE SCHEMA \"ventas\";");
        assert_eq!(schema_owner("Ven\"tas", " dq ana ").unwrap(), "ALTER SCHEMA \"Ven\"\"tas\" OWNER TO \"dq ana\";");
        assert!(schema_owner("ventas", "").is_err());
        assert!(create_schema(" ").is_err());
        assert_eq!(drop_schema("ventas").unwrap(), "DROP SCHEMA \"ventas\";");
        let spec = schema_spec();
        assert!(spec.owner && !spec.cascade);
        let g = script(&SecurityAction::Grant {
            privileges: spec.privileges.iter().map(|p| p.to_string()).collect(),
            object: obj("schema", None, "ventas"),
            to: "r".into(),
            grantable: true,
        })
        .unwrap();
        assert_eq!(g, "GRANT USAGE, CREATE, ALL PRIVILEGES ON SCHEMA \"ventas\" TO \"r\" WITH GRANT OPTION;");
    }
}

//! Users, roles and privileges of Dremio (docs/users-and-permissions.md).
//!
//! Enterprise (Software and Cloud) has role-based access: `sys.users`,
//! `sys.roles`, `sys.membership` (role, member, USER|ROLE) and
//! `sys.privileges` (grantee, privilege, object and its type), and SQL to
//! change them (`CREATE USER`, `ALTER USER … SET PASSWORD`, `CREATE ROLE`,
//! `GRANT … ON … TO USER|ROLE …`). Dremio OSS (Community) has none of it:
//! its users (read from `/apiv2/users/all`) are all administrators, and the
//! server answers the scripts that it's an Enterprise feature.
//!
//! Grants say whether the grantee is a user or a role (`TO ROLE r`), so
//! DBine names roles `role:<name>`.

use crate::ddl::{lit, q};
use crate::{text, DremioSession};
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SchemaSpec, SecurityAction, SecuritySpec};
use serde_json::{Map, Value};
use std::collections::{HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "ALTER", "MODIFY", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "DROP", "CREATE TABLE", "READ METADATA",
            "ALTER REFLECTION", "VIEW REFLECTION", "MANAGE GRANTS", "OWNERSHIP", "ALL", "CREATE USER", "CREATE ROLE",
            "CREATE SOURCE", "CREATE SPACE", "UPLOAD FILE", "VIEW JOB HISTORY", "EXPORT DIAGNOSTICS",
        ],
        // "" = SYSTEM; "schema" = a folder (`space.folder`).
        object_kinds: vec!["", "schema", kinds::TABLE, kinds::VIEW],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

const ROLE_PREFIX: &str = "role:";

fn role_name(r: &str) -> String {
    format!("{ROLE_PREFIX}{r}")
}

/// A principal's name as `(is role, name in Dremio)`.
fn grantee(name: &str) -> (bool, &str) {
    match name.strip_prefix(ROLE_PREFIX) {
        Some(r) => (true, r),
        None => (false, name),
    }
}

fn field(r: &Map<String, Value>, k: &str) -> String {
    r.get(k).map(text).unwrap_or_default()
}

const COMMUNITY: &str = "Dremio OSS (Community) no tiene roles ni permisos por objeto: todos sus usuarios son administradores. Los permisos son de Dremio Enterprise.";

// -- reading -------------------------------------------------------------------

pub async fn principals(s: &DremioSession) -> Result<Vec<Principal>> {
    let users = match s.records("SELECT * FROM sys.users").await {
        Ok(u) => u,
        Err(_) => return community_users(s).await,
    };
    let roles = s.records("SELECT * FROM sys.roles").await.unwrap_or_default();
    let members = s.records("SELECT role_name, member_name, member_type FROM sys.membership").await.unwrap_or_default();
    Ok(principals_of(&users, &roles, &members))
}

fn principals_of(users: &[Map<String, Value>], roles: &[Map<String, Value>], members: &[Map<String, Value>]) -> Vec<Principal> {
    let member_of = |name: &str, ty: &str| -> Vec<String> {
        members
            .iter()
            .filter(|m| field(m, "member_name") == name && field(m, "member_type").eq_ignore_ascii_case(ty))
            .map(|m| role_name(&field(m, "role_name")))
            .collect()
    };
    let mut out = Vec::new();
    for u in users {
        let name = field(u, "user_name");
        let member_of = member_of(&name, "USER");
        let mut details = Vec::new();
        for (k, label) in [("user_type", "Tipo"), ("status", "Estado"), ("created", "Alta"), ("created_by", "Origen")] {
            let v = field(u, k);
            if !v.is_empty() {
                details.push((label.to_string(), v));
            }
        }
        out.push(Principal {
            superuser: Some(member_of.iter().any(|r| r == "role:ADMIN")),
            can_login: Some(true),
            name,
            kind: PrincipalKind::User,
            member_of,
            details,
            ..Default::default()
        });
    }
    for r in roles {
        let name = field(r, "role_name");
        let ty = field(r, "role_type");
        out.push(Principal {
            member_of: member_of(&name, "ROLE"),
            superuser: Some(name == "ADMIN"),
            system: name == "ADMIN" || name == "PUBLIC",
            can_login: Some(false),
            details: if ty.is_empty() { Vec::new() } else { vec![("Tipo".into(), ty)] },
            name: role_name(&name),
            kind: PrincipalKind::Role,
            ..Default::default()
        });
    }
    out
}

async fn community_users(s: &DremioSession) -> Result<Vec<Principal>> {
    let v = s.conn.send(reqwest::Method::GET, "/apiv2/users/all", None).await?;
    Ok(v.get("users")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|u| {
            let c = u.get("userConfig").unwrap_or(u);
            let mut details = vec![("Nota".into(), "Dremio OSS: todos los usuarios son administradores".into())];
            let full = format!("{} {}", c.get("firstName").map(text).unwrap_or_default(), c.get("lastName").map(text).unwrap_or_default());
            if !full.trim().is_empty() {
                details.push(("Nombre".into(), full.trim().to_string()));
            }
            if let Some(e) = c.get("email").map(text).filter(|e| !e.is_empty()) {
                details.push(("Email".into(), e));
            }
            Principal {
                name: u.get("name").or_else(|| c.get("userName")).map(text).unwrap_or_default(),
                kind: PrincipalKind::User,
                can_login: Some(true),
                superuser: Some(true),
                disabled: c.get("active").and_then(Value::as_bool).map(|a| !a),
                details,
                ..Default::default()
            }
        })
        .collect())
}

/// `"a"."b c".t` → `a.b c.t`.
fn unquote_path(p: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                out.push('"');
            }
            '"' => quoted = !quoted,
            _ => out.push(c),
        }
    }
    out
}

fn grant_of(r: &Map<String, Value>, via: Option<String>) -> Grant {
    let ty = field(r, "object_type").to_uppercase();
    let (object, object_kind) = match ty.as_str() {
        "SYSTEM" | "" => (None, None),
        t => {
            let kind = match t {
                "PDS" | "TABLE" => kinds::TABLE,
                "VDS" | "VIEW" => kinds::VIEW,
                "FOLDER" => "schema",
                "FUNCTION" => kinds::FUNCTION,
                "SOURCE" => "source",
                "SPACE" => "space",
                "SCRIPT" => "script",
                other => return Grant { privilege: field(r, "privilege").replace('_', " "), object: Some(field(r, "object_id")), object_kind: Some(other.to_lowercase()), via, ..Default::default() },
            };
            (Some(unquote_path(&field(r, "object_id"))), Some(kind.to_string()))
        }
    };
    Grant { privilege: field(r, "privilege").replace('_', " "), object, object_kind, via, ..Default::default() }
}

pub async fn grants(s: &DremioSession, principal: &str) -> Result<Vec<Grant>> {
    let (is_role, name) = grantee(principal);
    let all = s.records("SELECT * FROM sys.privileges").await.map_err(|_| Error::Unsupported(COMMUNITY.into()))?;
    let members = s.records("SELECT role_name, member_name, member_type FROM sys.membership").await.unwrap_or_default();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([(name.to_string(), if is_role { "ROLE" } else { "USER" }, None::<String>)]);
    while let Some((n, ty, via)) = queue.pop_front() {
        if !seen.insert((n.clone(), ty)) || seen.len() > 64 {
            continue;
        }
        for r in all.iter().filter(|r| field(r, "grantee_id") == n && field(r, "grantee_type").eq_ignore_ascii_case(ty)) {
            out.push(grant_of(r, via.clone()));
        }
        for m in members.iter().filter(|m| field(m, "member_name") == n && field(m, "member_type").eq_ignore_ascii_case(ty)) {
            let role = field(m, "role_name");
            let v = via.clone().unwrap_or_else(|| role_name(&role));
            queue.push_back((role, "ROLE", Some(v)));
        }
    }
    Ok(out)
}

// -- scripts -------------------------------------------------------------------

fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let up = x.trim().to_uppercase().replace('_', " ");
        if up.is_empty() || !up.chars().all(|c| c.is_ascii_alphabetic() || c == ' ') {
            return Err(Error::Query(format!("«{x}» no es un permiso de Dremio")));
        }
        out.push(up);
    }
    Ok(out.join(", "))
}

/// A dotted path, each part quoted.
fn dotted(p: &str) -> String {
    p.split('.').map(q).collect::<Vec<_>>().join(".")
}

/// A folder path's parts: split at the dots, except inside a part written
/// between double quotes (`nessie."a.b"`, `""` for a quote in it), so a
/// folder whose name has a dot can be written.
fn path(p: &str) -> Result<Vec<String>> {
    let bad = || Error::Query(format!("la ruta «{p}» tiene comillas sin cerrar o texto después de una parte entre comillas"));
    let mut parts = Vec::new();
    let mut chars = p.trim().chars().peekable();
    loop {
        let mut part = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        part.push('"');
                    }
                    Some('"') => break,
                    Some(c) => part.push(c),
                    None => return Err(bad()),
                }
            }
            match chars.next() {
                None => {
                    parts.push(part);
                    return Ok(parts);
                }
                Some('.') => {}
                Some(_) => return Err(bad()),
            }
        } else {
            loop {
                match chars.next() {
                    Some('.') => break,
                    Some(c) => part.push(c),
                    None => {
                        parts.push(part);
                        return Ok(parts);
                    }
                }
            }
        }
        parts.push(part);
    }
}

/// A folder's path quoted, `None` when it has a single part (a space or a
/// source) or an empty one.
fn folder_path(p: &str) -> Result<Option<String>> {
    let parts = path(p)?;
    if parts.len() < 2 || parts.iter().any(|x| x.trim().is_empty()) {
        return Ok(None);
    }
    Ok(Some(parts.iter().map(|x| q(x)).collect::<Vec<_>>().join(".")))
}

/// `ON <object>`. The UI hands a listed grant's object back split at the
/// first dot, so the path is rebuilt whole.
fn on(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else { return Ok("SYSTEM".into()) };
    let full = match o.schema().filter(|s| !s.is_empty()) {
        Some(sc) => format!("{sc}.{}", o.name),
        None => o.name.clone(),
    };
    let what = match o.kind.as_str() {
        k if k == kinds::TABLE => "TABLE",
        k if k == kinds::VIEW => "VIEW",
        k if k == kinds::FUNCTION => "FUNCTION",
        "source" => "SOURCE",
        "space" => "SPACE",
        "schema" | "folder" => {
            return match folder_path(&full)? {
                Some(f) => Ok(format!("FOLDER {f}")),
                None => Err(Error::Query(format!(
                    "«{full}» es un espacio o un origen: escribí el script con ON SPACE o ON SOURCE en el editor"
                ))),
            };
        }
        other => return Err(Error::Unsupported(format!("DBine no otorga permisos sobre objetos «{other}» de Dremio"))),
    };
    Ok(format!("{what} {}", dotted(&full)))
}

fn to_whom(name: &str) -> String {
    match grantee(name) {
        (true, r) => format!("ROLE {}", q(r)),
        (false, u) => format!("USER {}", q(u)),
    }
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let mut s = format!("CREATE USER {};", q(name));
            if let Some(pw) = password.as_deref().filter(|p| !p.is_empty()) {
                s.push_str(&format!("\nALTER USER {} SET PASSWORD {};", q(name), lit(pw)));
            }
            s
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", q(grantee(name).1)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(grantee(name).1)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} SET PASSWORD {};", q(name), lit(password)),
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported("Dremio no deshabilita usuarios por SQL: hacelo desde la administración de usuarios de Dremio".into()))
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            let privs = privileges(p)?;
            let on = on(object)?;
            let is_folder = object.as_ref().is_some_and(|o| matches!(o.kind.as_str(), "schema" | "folder"));
            if *grantable && !is_folder {
                return Err(Error::Unsupported("Dremio no tiene WITH GRANT OPTION: otorgá MANAGE GRANTS sobre el objeto".into()));
            }
            let mut s = format!("GRANT {privs} ON {on} TO {};", to_whom(to));
            // "Con opción de otorgar" on a new folder ("Nuevo esquema…"):
            // Dremio's way is MANAGE GRANTS on it.
            if *grantable && !p.iter().any(|x| matches!(x.trim().to_uppercase().replace('_', " ").as_str(), "MANAGE GRANTS" | "OWNERSHIP")) {
                s.push_str(&format!("\n-- Dremio no tiene WITH GRANT OPTION: poder otorgar es MANAGE GRANTS sobre la carpeta.\nGRANT MANAGE GRANTS ON {on} TO {};", to_whom(to)));
            }
            s
        }
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {} ON {} FROM {};", privileges(p)?, on(object)?, to_whom(from)),
        SecurityAction::AddMember { role, member } => format!("GRANT ROLE {} TO {};", q(grantee(role).1), to_whom(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE ROLE {} FROM {};", q(grantee(role).1), to_whom(member)),
    })
}

// -- schemas (folders) ---------------------------------------------------------

/// "Nuevo esquema…": a schema is a folder, `CREATE FOLDER` / `DROP FOLDER`
/// by its whole path: the space or source the menu was opened on (taken
/// whole: a home space's name can have dots, `@ana.b`), then the folders
/// (`origen.carpeta`, as the explorer shows it, or just `carpeta`). Dremio
/// takes them in catalog sources (Nessie, Iceberg REST, Arctic); folders of
/// spaces are made in the UI or the REST API ("Create folder is not
/// supported for this source"). There's no owner clause (ownership is the
/// OWNERSHIP grant) and no CASCADE; grants on folders are Enterprise's.
pub fn schema_spec() -> SchemaSpec {
    SchemaSpec {
        owner: false,
        owner_kinds: dbine_driver::SchemaOwnerKinds::Both,
        cascade: false,
        privileges: vec![
            "SELECT", "ALTER", "CREATE TABLE", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "DROP", "VIEW REFLECTION",
            "ALTER REFLECTION", "MODIFY", "MANAGE GRANTS", "OWNERSHIP",
        ],
        grant_option: true,
    }
}

/// A folder's whole path, quoted. `database`: the space or source the
/// explorer menu was opened on, one part even with dots in it; `name` is
/// the explorer's schema (`origen.carpeta.sub`, starting with `database`)
/// or a path inside `database`. Without `database`, `name` is the whole
/// path and its first part is the space or source. The folders after the
/// space or source split at the dots, except inside a part written between
/// double quotes (`nessie."a.b"`): INFORMATION_SCHEMA writes a path with
/// plain dots, so a folder whose name has a dot (only possible in a
/// source) can't be told from nested folders unless quoted.
pub fn folder_in(database: Option<&str>, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Query("escribí el nombre del esquema".into()));
    }
    let top = database.map(str::trim).filter(|d| !d.is_empty());
    let parts = match top {
        Some(db) => {
            let rest = name.strip_prefix(db).and_then(|r| r.strip_prefix('.')).unwrap_or(if name == db { "" } else { name });
            let mut parts = vec![db.to_string()];
            if !rest.is_empty() {
                parts.extend(path(rest)?);
            }
            parts
        }
        None => path(name)?,
    };
    if parts.len() < 2 || parts.iter().any(|x| x.trim().is_empty()) {
        return Err(Error::Query(format!(
            "en Dremio un esquema es una carpeta: escribí su ruta completa, origen.carpeta (como se ve en el explorador), no «{name}»"
        )));
    }
    Ok(parts.iter().map(|x| q(x)).collect::<Vec<_>>().join("."))
}

pub fn create_schema(database: Option<&str>, name: &str) -> Result<String> {
    Ok(format!("CREATE FOLDER {};", folder_in(database, name)?))
}

pub fn drop_schema(database: Option<&str>, name: &str) -> Result<String> {
    Ok(format!("DROP FOLDER {};", folder_in(database, name)?))
}

/// A grant on the new folder, by its whole path (see `folder_in`).
pub fn schema_grant(database: Option<&str>, name: &str, privileges: &[String], to: &str, grantable: bool) -> Result<String> {
    let object = Some(ObjectRef { kind: "schema".into(), schema: None, name: folder_in(database, name)? });
    script(&SecurityAction::Grant { privileges: privileges.to_vec(), object, to: to.to_string(), grantable })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana\"b".into(), password: Some("p'w".into()) }),
            "CREATE USER \"ana\"\"b\";\nALTER USER \"ana\"\"b\" SET PASSWORD 'p''w';"
        );
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: None }), "CREATE USER \"ana\";");
        assert_eq!(s(SecurityAction::CreateRole { name: "lect".into() }), "CREATE ROLE \"lect\";");
        assert_eq!(s(SecurityAction::Drop { name: "role:lect".into(), kind: PrincipalKind::Role }), "DROP ROLE \"lect\";");
        assert_eq!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), "DROP USER \"ana\";");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "n".into() }), "ALTER USER \"ana\" SET PASSWORD 'n';");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "alter_reflection".into()], object: obj(kinds::TABLE, Some("sp.f"), "t"), to: "role:lect".into(), grantable: false }),
            "GRANT SELECT, ALTER REFLECTION ON TABLE \"sp\".\"f\".\"t\" TO ROLE \"lect\";"
        );
        // A listed grant comes back split at the first dot.
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: obj(kinds::VIEW, Some("sp"), "f.v"), from: "ana".into() }),
            "REVOKE SELECT ON VIEW \"sp\".\"f\".\"v\" FROM USER \"ana\";"
        );
        assert_eq!(s(SecurityAction::Grant { privileges: vec!["CREATE USER".into()], object: None, to: "ana".into(), grantable: false }), "GRANT CREATE USER ON SYSTEM TO USER \"ana\";");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("schema", None, "sp.f"), to: "ana".into(), grantable: false }),
            "GRANT SELECT ON FOLDER \"sp\".\"f\" TO USER \"ana\";"
        );
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("schema", None, "sp"), to: "a".into(), grantable: false }).is_err());
        assert_eq!(s(SecurityAction::AddMember { role: "role:lect".into(), member: "ana".into() }), "GRANT ROLE \"lect\" TO USER \"ana\";");
        assert_eq!(s(SecurityAction::RemoveMember { role: "role:lect".into(), member: "role:sub".into() }), "REVOKE ROLE \"lect\" FROM ROLE \"sub\";");
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT ON SYSTEM TO USER x; --".into()], object: None, to: "a".into(), grantable: false }).is_err());
        assert!(matches!(script(&SecurityAction::SetLogin { name: "a".into(), enabled: false }), Err(Error::Unsupported(_))));
    }

    fn rows(v: Value) -> Vec<Map<String, Value>> {
        v.as_array().unwrap().iter().map(|r| r.as_object().unwrap().clone()).collect()
    }

    #[test]
    fn reads_principals_and_grants() {
        let users = rows(json!([{"user_name": "ana", "user_type": "LOCAL", "status": "active"}, {"user_name": "admin"}]));
        let roles = rows(json!([{"role_name": "ADMIN", "role_type": "SYSTEM"}, {"role_name": "lect", "role_type": "LOCAL"}]));
        let members = rows(json!([
            {"role_name": "lect", "member_name": "ana", "member_type": "USER"},
            {"role_name": "ADMIN", "member_name": "admin", "member_type": "USER"},
        ]));
        let p = principals_of(&users, &roles, &members);
        let ana = p.iter().find(|p| p.name == "ana").unwrap();
        assert_eq!((ana.member_of.clone(), ana.superuser), (vec!["role:lect".to_string()], Some(false)));
        assert_eq!(p.iter().find(|p| p.name == "admin").unwrap().superuser, Some(true));
        assert!(p.iter().any(|p| p.name == "role:ADMIN" && p.system && p.kind == PrincipalKind::Role));
        let g = grant_of(&rows(json!([{"privilege": "SELECT", "object_id": "\"sp\".\"f\".\"t\"", "object_type": "PDS"}]))[0], None);
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref()), (Some("sp.f.t"), Some(kinds::TABLE)));
        let g = grant_of(&rows(json!([{"privilege": "CREATE_USER", "object_id": "", "object_type": "SYSTEM"}]))[0], Some("role:lect".into()));
        assert_eq!((g.privilege.as_str(), g.object, g.via.as_deref()), ("CREATE USER", None, Some("role:lect")));
    }

    #[test]
    fn folder_scripts() {
        let create_schema = |n: &str| super::create_schema(None, n);
        let drop_schema = |n: &str| super::drop_schema(None, n);
        assert_eq!(create_schema(" nessie.ventas ").unwrap(), "CREATE FOLDER \"nessie\".\"ventas\";");
        assert_eq!(drop_schema("nessie.ve\"n.sub").unwrap(), "DROP FOLDER \"nessie\".\"ve\"\"n\".\"sub\";");
        assert!(create_schema("ventas").is_err());
        assert!(create_schema("nessie.").is_err());
        assert!(drop_schema(" ").is_err());
        // With the menu's space or source: a bare folder goes in it, the
        // explorer's whole path keeps it as one part even with dots.
        assert_eq!(super::create_schema(Some("nessie"), "ventas").unwrap(), "CREATE FOLDER \"nessie\".\"ventas\";");
        assert_eq!(super::create_schema(Some("nessie"), "nessie.ventas").unwrap(), "CREATE FOLDER \"nessie\".\"ventas\";");
        assert_eq!(super::drop_schema(Some("@ana.b"), "@ana.b.f1.sub").unwrap(), "DROP FOLDER \"@ana.b\".\"f1\".\"sub\";");
        assert_eq!(super::drop_schema(Some("nessie"), "nessie.\"a.b\"").unwrap(), "DROP FOLDER \"nessie\".\"a.b\";");
        assert_eq!(super::create_schema(Some("nessie"), "\"a.b\".c").unwrap(), "CREATE FOLDER \"nessie\".\"a.b\".\"c\";");
        assert!(super::drop_schema(Some("nessie"), "nessie").is_err(), "the source itself isn't a folder");
        assert!(super::create_schema(Some("nessie"), "nessie.").is_err());
        assert_eq!(
            schema_grant(Some("@ana.b"), "@ana.b.f1", &["SELECT".into()], "role:lect", false).unwrap(),
            "GRANT SELECT ON FOLDER \"@ana.b\".\"f1\" TO ROLE \"lect\";"
        );
        let spec = schema_spec();
        assert!(!spec.owner && !spec.cascade);
        let g = script(&SecurityAction::Grant {
            privileges: spec.privileges.iter().map(|p| p.to_string()).collect(),
            object: obj("schema", None, "nessie.ventas"),
            to: "role:lect".into(),
            grantable: false,
        })
        .unwrap();
        assert!(g.starts_with("GRANT SELECT, ALTER, CREATE TABLE") && g.ends_with(" ON FOLDER \"nessie\".\"ventas\" TO ROLE \"lect\";"), "{g}");
        // A dot inside a folder's name, between double quotes.
        assert_eq!(create_schema("nessie.\"a.b\".c").unwrap(), "CREATE FOLDER \"nessie\".\"a.b\".\"c\";");
        assert_eq!(create_schema("\"nes\"\"sie\".x").unwrap(), "CREATE FOLDER \"nes\"\"sie\".\"x\";");
        assert!(create_schema("nessie.\"a.b").is_err());
        assert!(create_schema("nessie.\"a\"b").is_err());
        assert!(create_schema("\"nessie.ventas\"").is_err(), "one quoted part is a space or a source");
        // "Con opción de otorgar" on the new folder: MANAGE GRANTS on it.
        let grant = |p: &[&str]| {
            script(&SecurityAction::Grant {
                privileges: p.iter().map(|x| x.to_string()).collect(),
                object: obj("schema", None, "nessie.\"a.b\""),
                to: "ana".into(),
                grantable: true,
            })
            .unwrap()
        };
        assert_eq!(
            grant(&["SELECT"]),
            "GRANT SELECT ON FOLDER \"nessie\".\"a.b\" TO USER \"ana\";\n-- Dremio no tiene WITH GRANT OPTION: poder otorgar es MANAGE GRANTS sobre la carpeta.\nGRANT MANAGE GRANTS ON FOLDER \"nessie\".\"a.b\" TO USER \"ana\";"
        );
        assert_eq!(grant(&["SELECT", "manage_grants"]), "GRANT SELECT, MANAGE GRANTS ON FOLDER \"nessie\".\"a.b\" TO USER \"ana\";");
        // Elsewhere the grant option is still refused.
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("table", Some("nessie"), "t"), to: "ana".into(), grantable: true }).is_err());
    }
}

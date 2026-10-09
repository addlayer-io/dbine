//! Users, roles and permissions (docs/users-and-permissions.md) for OrientDB:
//! the database's `OUser` and `ORole` records. Users hold roles (`roles`),
//! a role may inherit another (`inheritedRole`), and permissions are the
//! roles' rules: a resource (`database.class.Cliente`, `database.schema`,
//! `database.function.*`…) with a bit mask (create 1, read 2, update 4,
//! delete 8, execute 16), changed with `GRANT` / `REVOKE … TO <role>`.
//! Server users (`orientdb-server-config.xml`, like `root`) aren't the
//! database's and aren't listed.
//!
//! Scripts use plain SQL on the records where there is no statement for it
//! (creating a role, a user's roles, status and password: OrientDB hashes
//! the password when the record is saved).

use crate::ddl::string;
use crate::{as_text, classify, OrientSession};
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::Value;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["READ", "CREATE", "UPDATE", "DELETE", "EXECUTE", "ALL", "NONE"],
        // "": every class (`database.class.*`).
        object_kinds: vec!["", crate::VERTEX, crate::EDGE, kinds::TABLE, kinds::FUNCTION],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: true,
    }
}

/// Explorer kind for other resources (`database.schema`, `database.cluster.x`…).
const RESOURCE: &str = "resource";

const BITS: &[(u64, &str)] = &[(1, "CREATE"), (2, "READ"), (4, "UPDATE"), (8, "DELETE"), (16, "EXECUTE")];

// -- reading ---------------------------------------------------------------

type Record = Vec<(String, Value)>;

fn field<'a>(r: &'a Record, k: &str) -> Option<&'a Value> {
    r.iter().find(|(n, _)| n == k).map(|(_, v)| v).filter(|v| !v.is_null())
}

fn text(r: &Record, k: &str) -> String {
    field(r, k).map(as_text).unwrap_or_default()
}

fn names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(as_text).filter(|s| !s.is_empty()).collect(),
        Some(v) => vec![as_text(v)].into_iter().filter(|s| !s.is_empty()).collect(),
        None => Vec::new(),
    }
}

async fn users(s: &OrientSession) -> Result<Vec<Record>> {
    Ok(s.command("SELECT name, status, roles.name AS roles FROM OUser ORDER BY name", -1).await?.records)
}

async fn roles(s: &OrientSession) -> Result<Vec<Record>> {
    Ok(s.command("SELECT name, mode, inheritedRole.name AS parent, rules FROM ORole ORDER BY name", -1).await?.records)
}

/// A role that can do everything: `ALLOW_ALL_BUT` mode or every right on `*`.
fn all_powerful(r: &Record) -> bool {
    field(r, "mode").and_then(Value::as_u64) == Some(1)
        || field(r, "rules").and_then(|x| x.get("*")).and_then(Value::as_u64) == Some(31)
}

fn is_system(name: &str) -> bool {
    matches!(name, "admin" | "reader" | "writer")
}

pub async fn principals(s: &OrientSession) -> Result<Vec<Principal>> {
    let roles = roles(s).await?;
    let admins: Vec<String> = roles.iter().filter(|r| all_powerful(r)).map(|r| text(r, "name")).collect();
    let mut out = Vec::new();
    for u in users(s).await? {
        let name = text(&u, "name");
        let status = text(&u, "status");
        let member_of = names(field(&u, "roles"));
        let mut details = vec![("Tipo".to_string(), "Usuario de la base (OUser)".to_string())];
        if !status.is_empty() {
            details.push(("Estado".into(), status.clone()));
        }
        out.push(Principal {
            superuser: Some(member_of.iter().any(|r| admins.contains(r))),
            can_login: Some(status != "SUSPENDED"),
            disabled: Some(status == "SUSPENDED"),
            member_of,
            details,
            system: false,
            kind: PrincipalKind::User,
            name,
        });
    }
    for r in &roles {
        let name = text(r, "name");
        let mode = match field(r, "mode").and_then(Value::as_u64) {
            Some(1) => "todo permitido salvo sus reglas (ALLOW_ALL_BUT)",
            _ => "todo denegado salvo sus reglas (DENY_ALL_BUT)",
        };
        out.push(Principal {
            superuser: Some(all_powerful(r)),
            can_login: None,
            disabled: None,
            member_of: names(field(r, "parent")),
            details: vec![("Tipo".into(), "Rol (ORole)".into()), ("Modo".into(), mode.into())],
            system: is_system(&name),
            kind: PrincipalKind::Role,
            name,
        });
    }
    Ok(out)
}

/// The privileges of a rule's mask.
fn mask_privileges(mask: u64) -> Vec<&'static str> {
    if mask & 31 == 31 {
        return vec!["ALL"];
    }
    if mask == 0 {
        return vec!["NONE"];
    }
    BITS.iter().filter(|(b, _)| mask & b != 0).map(|(_, n)| *n).collect()
}

/// A rule's resource as the object it applies to. Rules are stored in
/// lowercase: class names get their case back from `classes`.
fn resource_object(resource: &str, classes: &[(String, &'static str)]) -> (Option<String>, Option<String>) {
    // `database.class.*` is stored as `database.class`.
    if resource == "database.class" || resource == "database.class.*" {
        return (None, None);
    }
    if let Some(c) = resource.strip_prefix("database.class.").filter(|c| !c.contains('.') && *c != "*") {
        if let Some((name, kind)) = classes.iter().find(|(n, _)| n.eq_ignore_ascii_case(c)) {
            return (Some(name.clone()), Some(kind.to_string()));
        }
        return (Some(c.to_string()), Some(kinds::TABLE.into()));
    }
    if let Some(f) = resource.strip_prefix("database.function.").filter(|f| !f.contains('.') && *f != "*") {
        return (Some(f.to_string()), Some(kinds::FUNCTION.into()));
    }
    (Some(resource.to_string()), Some(RESOURCE.into()))
}

fn rule_grants(role: &Record, via: Option<&str>, classes: &[(String, &'static str)]) -> Vec<Grant> {
    let mut out = Vec::new();
    let Some(Value::Object(rules)) = field(role, "rules") else { return out };
    for (resource, mask) in rules {
        let (object, object_kind) = resource_object(resource, classes);
        for p in mask_privileges(mask.as_u64().unwrap_or(0)) {
            out.push(Grant {
                privilege: p.into(),
                object: object.clone(),
                object_kind: object_kind.clone(),
                grantable: false,
                denied: p == "NONE",
                via: via.map(str::to_string),
            });
        }
    }
    out
}

pub async fn grants(s: &OrientSession, principal: &str) -> Result<Vec<Grant>> {
    let roles = roles(s).await?;
    let classes: Vec<(String, &'static str)> = match s.metadata().await {
        Ok(m) => classify(m.get("classes").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]))
            .into_iter()
            .map(|(n, k, _)| (n, k))
            .collect(),
        Err(_) => Vec::new(),
    };
    let role = |n: &str| roles.iter().find(|r| text(r, "name") == n);
    // (role, the direct role it comes through; None: the principal itself)
    let mut queue: Vec<(String, Option<String>)> = Vec::new();
    match role(principal) {
        Some(_) => queue.push((principal.to_string(), None)),
        None => {
            let us = users(s).await?;
            let u = us.iter().find(|u| text(u, "name") == principal).ok_or_else(|| Error::Query(format!("no existe «{principal}»")))?;
            queue.extend(names(field(u, "roles")).into_iter().map(|r| (r.clone(), Some(r))));
        }
    }
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    while let Some((name, via)) = queue.pop() {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        let Some(r) = role(&name) else { continue };
        out.extend(rule_grants(r, via.as_deref(), &classes));
        for parent in names(field(r, "parent")) {
            queue.push((parent.clone(), Some(via.clone().unwrap_or(parent))));
        }
    }
    out.sort_by(|a, b| {
        (a.via.is_some(), &a.object_kind, &a.object, &a.privilege).cmp(&(b.via.is_some(), &b.object_kind, &b.object, &b.privilege))
    });
    out.dedup();
    Ok(out)
}

// -- scripts ---------------------------------------------------------------

/// A role name in `GRANT … TO`: plain, or between backticks.
fn role_ident(name: &str) -> Result<String> {
    if name.is_empty() || name.contains(['`', '\\', '\n', '\r']) {
        return Err(Error::Query(format!("«{name}» no es un nombre de rol válido")));
    }
    Ok(crate::ident(name))
}

/// A resource name piece: letters, digits, `_`, `-` and `*`.
fn piece(name: &str) -> Result<&str> {
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || "_-*".contains(c)) {
        return Err(Error::Query(format!("«{name}» no se puede usar en un recurso de OrientDB")));
    }
    Ok(name)
}

/// The resource a privilege applies to.
fn resource(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else { return Ok("database.class.*".into()) };
    Ok(match o.kind.as_str() {
        kinds::FUNCTION => format!("database.function.{}", piece(&o.name)?),
        RESOURCE => {
            let full = match o.schema() {
                Some(sc) => format!("{sc}.{}", o.name),
                None => o.name.clone(),
            };
            for p in full.split('.') {
                piece(p)?;
            }
            full
        }
        _ => format!("database.class.{}", piece(&o.name)?),
    })
}

fn privileges(p: &[String]) -> Result<Vec<String>> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    p.iter()
        .map(|x| {
            let x = x.trim().to_uppercase();
            if ["NONE", "CREATE", "READ", "UPDATE", "DELETE", "EXECUTE", "ALL"].contains(&x.as_str()) {
                Ok(x)
            } else {
                Err(Error::Query(format!("«{x}» no es un permiso de OrientDB (NONE, CREATE, READ, UPDATE, DELETE, EXECUTE, ALL)")))
            }
        })
        .collect()
}

fn role_record(name: &str) -> String {
    format!("(SELECT FROM ORole WHERE name = {})", string(name))
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!(
                "-- Sin roles no puede hacer nada: agregalo a uno (reader, writer…).\nINSERT INTO OUser SET name = {}, password = {}, status = 'ACTIVE', roles = [];",
                string(name),
                string(pw)
            )
        }
        SecurityAction::CreateRole { name } => format!("INSERT INTO ORole SET name = {}, mode = 0;", string(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DELETE FROM OUser WHERE name = {};", string(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DELETE FROM ORole WHERE name = {};", string(name)),
        SecurityAction::SetPassword { name, password } => {
            format!("UPDATE OUser SET password = {} WHERE name = {};", string(password), string(name))
        }
        SecurityAction::SetLogin { name, enabled } => format!(
            "UPDATE OUser SET status = '{}' WHERE name = {};",
            if *enabled { "ACTIVE" } else { "SUSPENDED" },
            string(name)
        ),
        SecurityAction::Grant { grantable: true, .. } => {
            return Err(Error::Query("OrientDB no tiene la opción de otorgar a otros (WITH GRANT OPTION)".into()))
        }
        SecurityAction::Grant { privileges: p, object, to, .. } => {
            let (res, role) = (resource(object)?, role_ident(to)?);
            let lines: Vec<String> = privileges(p)?.iter().map(|p| format!("GRANT {p} ON {res} TO {role};")).collect();
            format!("-- Los permisos son de los roles: «{}» tiene que ser un rol.\n{}", crate::comment_text(to), lines.join("\n"))
        }
        SecurityAction::Revoke { privileges: p, object, from } => {
            let (res, role) = (resource(object)?, role_ident(from)?);
            privileges(p)?.iter().map(|p| format!("REVOKE {p} ON {res} FROM {role};")).collect::<Vec<_>>().join("\n")
        }
        SecurityAction::AddMember { role, member } => {
            format!("UPDATE OUser SET roles = roles || {} WHERE name = {};", role_record(role), string(member))
        }
        SecurityAction::RemoveMember { role, member } => {
            format!("UPDATE OUser REMOVE roles = {} WHERE name = {};", role_record(role), string(member))
        }
    })
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
        assert!(s(SecurityAction::CreateUser { name: "bo b".into(), password: Some("p'w\\".into()) })
            .ends_with("\nINSERT INTO OUser SET name = 'bo b', password = 'p\\'w\\\\', status = 'ACTIVE', roles = [];"));
        assert!(script(&SecurityAction::CreateUser { name: "a".into(), password: None }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "ven'tas".into() }), "INSERT INTO ORole SET name = 'ven\\'tas', mode = 0;");
        assert_eq!(s(SecurityAction::Drop { name: "a".into(), kind: PrincipalKind::User }), "DELETE FROM OUser WHERE name = 'a';");
        assert_eq!(s(SecurityAction::Drop { name: "r".into(), kind: PrincipalKind::Role }), "DELETE FROM ORole WHERE name = 'r';");
        assert_eq!(s(SecurityAction::SetPassword { name: "a".into(), password: "n".into() }), "UPDATE OUser SET password = 'n' WHERE name = 'a';");
        assert_eq!(s(SecurityAction::SetLogin { name: "a".into(), enabled: false }), "UPDATE OUser SET status = 'SUSPENDED' WHERE name = 'a';");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["read".into(), "UPDATE".into()], object: obj("vertex", None, "Cliente"), to: "mi rol".into(), grantable: false }),
            "-- Los permisos son de los roles: «mi rol» tiene que ser un rol.\nGRANT READ ON database.class.Cliente TO `mi rol`;\nGRANT UPDATE ON database.class.Cliente TO `mi rol`;"
        );
        assert!(s(SecurityAction::Grant { privileges: vec!["READ".into()], object: None, to: "r".into(), grantable: false }).ends_with("GRANT READ ON database.class.* TO r;"));
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["EXECUTE".into()], object: obj("function", None, "calc"), from: "r".into() }),
            "REVOKE EXECUTE ON database.function.calc FROM r;"
        );
        // A resource as the UI gives it back: split at the first dot.
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["READ".into()], object: obj("resource", Some("database"), "cluster.internal"), from: "r".into() }),
            "REVOKE READ ON database.cluster.internal FROM r;"
        );
        assert_eq!(s(SecurityAction::Revoke { privileges: vec!["READ".into()], object: obj("resource", None, "database"), from: "r".into() }), "REVOKE READ ON database FROM r;");
        assert_eq!(
            s(SecurityAction::AddMember { role: "ventas".into(), member: "ana".into() }),
            "UPDATE OUser SET roles = roles || (SELECT FROM ORole WHERE name = 'ventas') WHERE name = 'ana';"
        );
        assert_eq!(
            s(SecurityAction::RemoveMember { role: "ventas".into(), member: "ana".into() }),
            "UPDATE OUser REMOVE roles = (SELECT FROM ORole WHERE name = 'ventas') WHERE name = 'ana';"
        );
        let g = |p: &str, o, to: &str| script(&SecurityAction::Grant { privileges: vec![p.into()], object: o, to: to.into(), grantable: false });
        assert!(g("READ; DROP CLASS x", None, "r").is_err());
        assert!(g("READ", obj("table", None, "a b"), "r").is_err());
        assert!(g("READ", obj("table", None, "a;DELETE"), "r").is_err());
        assert!(g("READ", None, "r`x").is_err());
    }

    #[test]
    fn reads_rules() {
        let role: Record = vec![
            ("name".into(), json!("ventas")),
            ("rules".into(), json!({ "database.class.cliente": 6, "database.class": 2, "database": 31, "database.function.calc": 16, "database.cluster.x": 0 })),
        ];
        let classes = vec![("Cliente".to_string(), crate::VERTEX)];
        let mut g = rule_grants(&role, Some("ventas"), &classes);
        g.sort_by(|a, b| (&a.object, &a.privilege).cmp(&(&b.object, &b.privilege)));
        let got: Vec<(&str, Option<&str>, Option<&str>, bool)> =
            g.iter().map(|g| (g.privilege.as_str(), g.object.as_deref(), g.object_kind.as_deref(), g.denied)).collect();
        assert_eq!(
            got,
            [
                ("READ", None, None, false),
                ("READ", Some("Cliente"), Some("vertex"), false),
                ("UPDATE", Some("Cliente"), Some("vertex"), false),
                ("EXECUTE", Some("calc"), Some("function"), false),
                ("ALL", Some("database"), Some("resource"), false),
                ("NONE", Some("database.cluster.x"), Some("resource"), true),
            ]
        );
        assert!(g.iter().all(|g| g.via.as_deref() == Some("ventas")));
    }
}

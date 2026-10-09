//! Users, roles and permissions (docs/users-and-permissions.md). Cassandra
//! and ScyllaDB have roles only: a role that can log in is a user. Roles
//! are server-wide and hold other roles; permissions apply to all keyspaces,
//! a keyspace, a table, roles, functions or MBeans. It needs
//! `PasswordAuthenticator` and `CassandraAuthorizer` in cassandra.yaml
//! (scylla.yaml); Amazon Keyspaces uses AWS IAM instead.

use crate::{boolean, cql, text, texts, CassandraSession, Flavor};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};

pub const KEYSPACES: &str = "Amazon Keyspaces controla el acceso con IAM de AWS: los usuarios y permisos se administran en IAM, no en la base";

const NO_AUTH: &str = "Este servidor no tiene autenticación: usa AllowAllAuthenticator, así que no hay usuarios ni roles. \
Para administrarlos activá `authenticator: PasswordAuthenticator` y `authorizer: CassandraAuthorizer` en cassandra.yaml (scylla.yaml en ScyllaDB) y reiniciá.";

const NO_AUTHORIZER: &str = "Este servidor no controla permisos: usa AllowAllAuthorizer, así que todo usuario puede hacer todo. \
Para administrarlos activá `authorizer: CassandraAuthorizer` en cassandra.yaml (scylla.yaml en ScyllaDB) y reiniciá.";

pub fn spec(f: Flavor) -> Option<SecuritySpec> {
    (f != Flavor::Keyspaces).then(|| SecuritySpec {
        privileges: vec!["SELECT", "MODIFY", "CREATE", "ALTER", "DROP", "AUTHORIZE", "DESCRIBE", "EXECUTE", "UNMASK", "SELECT_MASKED", "ALL PERMISSIONS"],
        object_kinds: vec!["", "table"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    })
}

// -- reading -----------------------------------------------------------------

/// The server's refusal when authentication or authorization is off, explained.
fn explain(e: Error, authorizer: bool) -> Error {
    match e {
        Error::Query(m) => {
            let l = m.to_lowercase();
            if l.contains("anonymous") || l.contains("allowallauthenticator") || l.contains("not logged in") {
                Error::Unsupported(NO_AUTH.into())
            } else if l.contains("allowallauthorizer") || (authorizer && l.contains("not supported")) {
                Error::Unsupported(NO_AUTHORIZER.into())
            } else {
                Error::Query(m)
            }
        }
        e => e,
    }
}

struct RoleRow {
    name: String,
    superuser: bool,
    login: bool,
    member_of: Option<Vec<String>>,
    password: Option<bool>,
}

/// The roles table (a superuser can read it): memberships and whether a
/// role has a password. Cassandra keeps it in `system_auth`; ScyllaDB 6+
/// in `system`.
async fn roles_table(s: &CassandraSession) -> Option<Vec<RoleRow>> {
    let tables: &[&str] = if s.flavor == Flavor::Scylla { &["system.roles", "system_auth.roles"] } else { &["system_auth.roles"] };
    for t in tables {
        let q = format!("SELECT role, is_superuser, can_login, member_of, salted_hash FROM {t}");
        if let Ok(rows) = s.rows(&q, ()).await {
            if rows.is_empty() {
                continue;
            }
            return Some(
                rows.iter()
                    .map(|r| RoleRow {
                        name: text(r, 0),
                        superuser: boolean(r, 1),
                        login: boolean(r, 2),
                        member_of: Some(texts(r, 3)),
                        password: Some(!text(r, 4).is_empty()),
                    })
                    .collect(),
            );
        }
    }
    None
}

pub async fn principals(s: &CassandraSession) -> Result<Vec<Principal>> {
    if s.flavor == Flavor::Keyspaces {
        return Err(Error::Unsupported(KEYSPACES.into()));
    }
    // LIST ROLES proves authentication is on (and works for non-superusers).
    let listed = s.rows("LIST ROLES", ()).await.map_err(|e| explain(e, false))?;
    let roles = match roles_table(s).await {
        Some(r) => r,
        None => {
            let mut out = Vec::new();
            for r in &listed {
                let name = text(r, 0);
                let q = format!("LIST ROLES OF {} NORECURSIVE", cql::ident(&name));
                let member_of = s.rows(&q, ()).await.ok().map(|v| v.iter().map(|r| text(r, 0)).filter(|x| *x != name).collect());
                out.push(RoleRow { superuser: boolean(r, 1), login: boolean(r, 2), member_of, password: None, name });
            }
            out
        }
    };
    let mut out: Vec<Principal> = roles
        .into_iter()
        .map(|r| {
            // A role that can log in, or could (it has a password), is a user.
            let user = r.login || r.password == Some(true);
            let mut details = vec![("Tipo".to_string(), if user { "Rol con ingreso (usuario)" } else { "Rol" }.to_string())];
            if let Some(p) = r.password {
                details.push(("Contraseña".into(), if p { "Sí" } else { "No" }.into()));
            }
            Principal {
                kind: if user { PrincipalKind::User } else { PrincipalKind::Role },
                can_login: Some(r.login),
                superuser: Some(r.superuser),
                disabled: user.then_some(!r.login),
                member_of: r.member_of.unwrap_or_default(),
                details,
                system: r.name == "cassandra",
                name: r.name,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub async fn grants(s: &CassandraSession, principal: &str) -> Result<Vec<Grant>> {
    if s.flavor == Flavor::Keyspaces {
        return Err(Error::Unsupported(KEYSPACES.into()));
    }
    let q = format!("LIST ALL PERMISSIONS OF {}", cql::ident(principal));
    let rows = s.rows(&q, ()).await.map_err(|e| explain(e, true))?;
    // role | username | resource | permission; `role` is who holds it.
    Ok(rows
        .iter()
        .map(|r| {
            let holder = text(r, 0);
            let (object, object_kind) = resource(&text(r, 2));
            Grant {
                privilege: text(r, 3),
                object,
                object_kind,
                grantable: false,
                denied: false,
                via: (holder != principal).then_some(holder),
            }
        })
        .collect())
}

/// `<table ks.t>` → ("ks.t", "table"), so revoking it round-trips.
fn resource(r: &str) -> (Option<String>, Option<String>) {
    let inner = r.trim().trim_start_matches('<').trim_end_matches('>');
    let some = |o: &str, k: &str| (Some(o.to_string()), Some(k.to_string()));
    if inner == "all keyspaces" {
        return (None, None);
    }
    if let Some(ks) = inner.strip_prefix("all functions in ") {
        return some(&format!("ALL FUNCTIONS IN KEYSPACE {ks}"), "resource");
    }
    match inner {
        "all roles" => return some("ALL ROLES", "resource"),
        "all functions" => return some("ALL FUNCTIONS", "resource"),
        "all mbeans" => return some("ALL MBEANS", "resource"),
        _ => {}
    }
    for k in ["keyspace", "table", "role", "function", "mbean"] {
        if let Some(o) = inner.strip_prefix(k).and_then(|o| o.strip_prefix(' ')) {
            return some(o, k);
        }
    }
    (Some(inner.to_string()), Some("resource".into()))
}

// -- scripts -----------------------------------------------------------------

fn name(n: &str) -> Result<String> {
    let n = n.trim();
    if n.is_empty() {
        return Err(Error::Query("escribí un nombre".into()));
    }
    Ok(cql::ident(n))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn password(p: Option<&str>) -> Result<String> {
    p.filter(|p| !p.is_empty()).map(lit).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))
}

/// Permission names: letters and `_`; `ALL` is `ALL PERMISSIONS`.
fn permissions(p: &[String]) -> Result<Vec<String>> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    p.iter()
        .map(|x| {
            let up = x.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_uppercase();
            if up.is_empty() || !up.chars().all(|c| c.is_ascii_alphabetic() || c == '_' || c == ' ') {
                return Err(Error::Query(format!("«{x}» no es un permiso de Cassandra")));
            }
            Ok(if up == "ALL" { "ALL PERMISSIONS".into() } else { up })
        })
        .collect()
}

/// The resource a permission applies to (`None`: all keyspaces).
fn on(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else { return Ok("ALL KEYSPACES".into()) };
    let bad = || Error::Query(format!("«{}» no es un recurso de Cassandra", o.name));
    Ok(match o.kind.as_str() {
        "keyspace" | "database" | "schema" => format!("KEYSPACE {}", name(&o.name)?),
        // Without a keyspace, the session's.
        "table" | "view" => format!("TABLE {}", cql::qualified(o.schema(), &o.name)),
        "role" => format!("ROLE {}", name(&o.name)?),
        "mbean" => format!("MBEAN {}", lit(&o.name)),
        "function" => {
            // `ks.f(int, text)`: the signature as the server listed it.
            let ok = |c: char| c.is_ascii_alphanumeric() || "_(), <>".contains(c);
            if !o.name.chars().all(ok) {
                return Err(bad());
            }
            match o.schema() {
                Some(ks) => format!("FUNCTION {}.{}", cql::ident(ks), o.name),
                None => format!("FUNCTION {}", o.name),
            }
        }
        "resource" => {
            let n = o.name.trim();
            match n {
                "ALL ROLES" | "ALL FUNCTIONS" | "ALL MBEANS" | "ALL KEYSPACES" => n.to_string(),
                _ => match n.strip_prefix("ALL FUNCTIONS IN KEYSPACE ") {
                    Some(ks) => format!("ALL FUNCTIONS IN KEYSPACE {}", name(ks)?),
                    None => return Err(bad()),
                },
            }
        }
        k => return Err(Error::Query(format!("Cassandra no otorga permisos sobre objetos «{k}»"))),
    })
}

pub fn script(f: Flavor, a: &SecurityAction) -> Result<String> {
    if f == Flavor::Keyspaces {
        return Err(Error::Unsupported(KEYSPACES.into()));
    }
    Ok(match a {
        SecurityAction::CreateUser { name: n, password: p } => {
            format!("CREATE ROLE {} WITH PASSWORD = {} AND LOGIN = true;", name(n)?, password(p.as_deref())?)
        }
        SecurityAction::CreateRole { name: n } => format!("CREATE ROLE {};", name(n)?),
        SecurityAction::Drop { name: n, .. } => format!("DROP ROLE {};", name(n)?),
        SecurityAction::SetPassword { name: n, password: p } => format!("ALTER ROLE {} WITH PASSWORD = {};", name(n)?, password(Some(p))?),
        SecurityAction::SetLogin { name: n, enabled } => format!("ALTER ROLE {} WITH LOGIN = {enabled};", name(n)?),
        SecurityAction::Grant { privileges, object, to, grantable } => {
            if *grantable {
                return Err(Error::Unsupported(
                    "Cassandra no tiene «con opción de otorgar»: otorgá además el permiso AUTHORIZE sobre el mismo recurso".into(),
                ));
            }
            let (on, to) = (on(object)?, name(to)?);
            permissions(privileges)?.iter().map(|p| format!("GRANT {p} ON {on} TO {to};")).collect::<Vec<_>>().join("\n")
        }
        SecurityAction::Revoke { privileges, object, from } => {
            let (on, from) = (on(object)?, name(from)?);
            permissions(privileges)?.iter().map(|p| format!("REVOKE {p} ON {on} FROM {from};")).collect::<Vec<_>>().join("\n")
        }
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", name(role)?, name(member)?),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", name(role)?, name(member)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(Flavor::Cassandra, &a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "Ana".into(), password: Some("p'w".into()) }),
            "CREATE ROLE \"Ana\" WITH PASSWORD = 'p''w' AND LOGIN = true;"
        );
        assert_eq!(s(SecurityAction::CreateRole { name: "lect".into() }), "CREATE ROLE lect;");
        assert_eq!(s(SecurityAction::Drop { name: "lect".into(), kind: PrincipalKind::Role }), "DROP ROLE lect;");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }), "ALTER ROLE ana WITH PASSWORD = 'x';");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ALTER ROLE ana WITH LOGIN = false;");
        assert_eq!(s(SecurityAction::AddMember { role: "lect".into(), member: "ana".into() }), "GRANT lect TO ana;");
        assert_eq!(s(SecurityAction::RemoveMember { role: "lect".into(), member: "ana".into() }), "REVOKE lect FROM ana;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "all".into()], object: None, to: "lect".into(), grantable: false }),
            "GRANT SELECT ON ALL KEYSPACES TO lect;\nGRANT ALL PERMISSIONS ON ALL KEYSPACES TO lect;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["MODIFY".into()], object: obj("table", Some("ks"), "T"), to: "ana".into(), grantable: false }),
            "GRANT MODIFY ON TABLE ks.\"T\" TO ana;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: obj("keyspace", None, "ks"), from: "lect".into() }),
            "REVOKE SELECT ON KEYSPACE ks FROM lect;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["EXECUTE".into()], object: obj("resource", None, "ALL FUNCTIONS IN KEYSPACE ks"), from: "r".into() }),
            "REVOKE EXECUTE ON ALL FUNCTIONS IN KEYSPACE ks FROM r;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["EXECUTE".into()], object: obj("function", Some("ks"), "f(int, text)"), from: "r".into() }),
            "REVOKE EXECUTE ON FUNCTION ks.f(int, text) FROM r;"
        );
        let e = |a| script(Flavor::Cassandra, &a).unwrap_err();
        assert!(e(SecurityAction::Grant { privileges: vec!["SELECT; DROP".into()], object: None, to: "r".into(), grantable: false }).to_string().contains("no es un permiso"));
        assert!(e(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("resource", None, "ALL x; DROP"), to: "r".into(), grantable: false }).to_string().contains("no es un recurso"));
        assert!(e(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("function", None, "f'); x"), to: "r".into(), grantable: false }).to_string().contains("no es un recurso"));
        assert!(matches!(e(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: None, to: "r".into(), grantable: true }), Error::Unsupported(_)));
        assert!(matches!(e(SecurityAction::CreateUser { name: "a".into(), password: None }), Error::Query(_)));
        assert!(matches!(script(Flavor::Keyspaces, &SecurityAction::CreateRole { name: "a".into() }), Err(Error::Unsupported(_))));
        assert!(spec(Flavor::Keyspaces).is_none() && spec(Flavor::Scylla).is_some());
    }

    #[test]
    fn resources() {
        assert_eq!(resource("<all keyspaces>"), (None, None));
        assert_eq!(resource("<table ks.t>"), (Some("ks.t".into()), Some("table".into())));
        assert_eq!(resource("<keyspace ks>"), (Some("ks".into()), Some("keyspace".into())));
        assert_eq!(resource("<all functions in ks>"), (Some("ALL FUNCTIONS IN KEYSPACE ks".into()), Some("resource".into())));
        assert_eq!(resource("<role ana>"), (Some("ana".into()), Some("role".into())));
        assert_eq!(resource("<all mbeans>"), (Some("ALL MBEANS".into()), Some("resource".into())));
    }
}

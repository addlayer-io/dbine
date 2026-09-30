//! RisingWave: users only (no roles, so no membership), kept in
//! `rw_catalog.rw_users`. Privileges are in the `acl` column of the
//! `rw_catalog` relations (tables, materialized views, views, sources,
//! sinks), schemas and databases, as PostgreSQL-style ACL items
//! (`ana=r*w/root`) whose names are never quoted: they're matched against
//! the server's user names (grantee and grantor) rather than split blindly.
//!
//! Database grants go on a named database (kind "database"), not on ""
//! (the current one): `GRANT … ON DATABASE` needs the name, and RisingWave
//! has no `DO` blocks to look it up when the script runs.

use super::{privileges, q, yes};
use crate::catalog::cell;
use crate::session::PgSession;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashMap;

pub(super) fn spec(object_kinds: Vec<&'static str>) -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "USAGE", "CREATE", "CONNECT", "ALL PRIVILEGES"],
        // A named database isn't an explorer kind, so it isn't filtered.
        object_kinds: std::iter::once("database").chain(object_kinds).collect(),
        create_user: true,
        create_role: false,
        passwords: true,
        membership: false,
        per_database: false,
    }
}

pub(super) async fn principals(s: &PgSession) -> Result<Vec<Principal>> {
    // `SELECT *`: `is_admin` came in later versions.
    let rows = s.text("SELECT * FROM rw_catalog.rw_users ORDER BY name").await?;
    Ok(rows.iter().map(|r| user(cell(r, "name").unwrap_or_default(), |c| yes(r, c))).collect())
}

/// A `rw_users` row; `flag` reads its boolean columns.
fn user(name: String, flag: impl Fn(&str) -> bool) -> Principal {
    let login = flag("can_login");
    let mut details = vec![("Tipo".to_string(), "Usuario".to_string())];
    for (column, label) in [
        ("create_db", "Puede crear bases"),
        ("create_user", "Puede crear usuarios"),
        ("is_admin", "Administrador del clúster"),
    ] {
        if flag(column) {
            details.push((label.into(), "sí".into()));
        }
    }
    Principal {
        kind: PrincipalKind::User,
        can_login: Some(login),
        superuser: Some(flag("is_super")),
        disabled: Some(!login),
        member_of: Vec::new(),
        details,
        // Created with the cluster.
        system: matches!(name.as_str(), "root" | "postgres" | "rwadmin"),
        name,
    }
}

const SYSTEM_SCHEMAS: &str = "('pg_catalog', 'information_schema', 'rw_catalog')";

pub(super) async fn grants(s: &PgSession, via: &HashMap<String, Option<String>>) -> Result<Vec<(String, Grant)>> {
    let relation = |table: &str, kind: &str| {
        format!(
            "SELECT s.name || '.' || o.name AS object, '{kind}' AS kind, u.name AS owner, unnest(o.acl) AS item
               FROM rw_catalog.{table} o
               JOIN rw_catalog.rw_schemas s ON s.id = o.schema_id
               LEFT JOIN rw_catalog.rw_users u ON u.id = o.owner
              WHERE s.name NOT IN {SYSTEM_SCHEMAS}"
        )
    };
    let queries = [
        relation("rw_tables", "table"),
        relation("rw_materialized_views", "materialized_view"),
        relation("rw_views", "view"),
        relation("rw_sources", "source"),
        relation("rw_sinks", "sink"),
        format!(
            "SELECT s.name AS object, 'schema' AS kind, u.name AS owner, unnest(s.acl) AS item
               FROM rw_catalog.rw_schemas s LEFT JOIN rw_catalog.rw_users u ON u.id = s.owner
              WHERE s.name NOT IN {SYSTEM_SCHEMAS}"
        ),
        "SELECT d.name AS object, 'database' AS kind, u.name AS owner, unnest(d.acl) AS item
           FROM rw_catalog.rw_databases d LEFT JOIN rw_catalog.rw_users u ON u.id = d.owner
          WHERE d.name = current_database()"
            .to_string(),
    ];
    // Every user, not just the ones looked up: an item is only told apart
    // from another user's (`a=r/b=w/root` is user `a=r/b`'s) by knowing them.
    let users: Vec<String> = s.text("SELECT name FROM rw_catalog.rw_users").await?.iter().filter_map(|r| cell(r, "name")).collect();
    let names: Vec<&str> = users.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for sql in queries {
        for r in s.text(&sql).await? {
            let (Some(item), Some(object), Some(kind)) = (cell(&r, "item"), cell(&r, "object"), cell(&r, "kind")) else { continue };
            let Some(acl) = parse_acl_item(&item, &names).filter(|a| via.contains_key(&a.grantee)) else { continue };
            // What the owner holds by owning it.
            if cell(&r, "owner").is_some_and(|o| o == acl.grantee && o == acl.grantor) {
                continue;
            }
            out.extend(acl.privileges.iter().map(|(p, grantable)| {
                (
                    acl.grantee.clone(),
                    Grant {
                        privilege: p.to_string(),
                        object: Some(object.clone()),
                        object_kind: Some(kind.clone()),
                        grantable: *grantable,
                        ..Default::default()
                    },
                )
            }));
        }
    }
    Ok(out)
}

#[derive(Debug, PartialEq)]
struct AclItem {
    grantee: String,
    /// (privilege, with grant option)
    privileges: Vec<(&'static str, bool)>,
    grantor: String,
}

fn privilege(letter: char) -> Option<&'static str> {
    Some(match letter {
        'r' => "SELECT",
        'w' => "UPDATE",
        'a' => "INSERT",
        'd' => "DELETE",
        'D' => "TRUNCATE",
        'x' => "REFERENCES",
        't' => "TRIGGER",
        'X' => "EXECUTE",
        'U' => "USAGE",
        'C' => "CREATE",
        'c' => "CONNECT",
        'T' => "TEMPORARY",
        _ => return None,
    })
}

/// `grantee=privileges/grantor`, both among `names` (every user: RisingWave
/// doesn't quote the names, so `a=b=r/root` is user `a=b`'s SELECT, not
/// user `a`'s, and `a=r/b=w/root` is user `a=r/b`'s UPDATE).
fn parse_acl_item(item: &str, names: &[&str]) -> Option<AclItem> {
    names.iter().find_map(|name| {
        let rest = item.strip_prefix(name)?.strip_prefix('=')?;
        let (letters, grantor) = rest.split_once('/')?;
        if letters.is_empty() || !letters.chars().all(|c| c.is_ascii_alphabetic() || c == '*') || !names.contains(&grantor) {
            return None;
        }
        let mut privileges: Vec<(&'static str, bool)> = Vec::new();
        for c in letters.chars() {
            if c == '*' {
                if let Some(last) = privileges.last_mut() {
                    last.1 = true;
                }
            } else if let Some(p) = privilege(c) {
                privileges.push((p, false));
            }
        }
        Some(AclItem { grantee: name.to_string(), privileges, grantor: grantor.to_string() })
    })
}

/// `TABLE "s"."t"`, `MATERIALIZED VIEW …`, `SOURCE …`, `SINK …`, `SCHEMA "s"`.
fn target(o: &ObjectRef) -> String {
    let name = || qualified_name(Quote::Double, o.schema(), &o.name);
    match o.kind.as_str() {
        "database" => format!("DATABASE {}", q(&o.name)),
        "schema" => format!("SCHEMA {}", q(&o.name)),
        "materialized_view" => format!("MATERIALIZED VIEW {}", name()),
        "view" => format!("VIEW {}", name()),
        "source" => format!("SOURCE {}", name()),
        "sink" => format!("SINK {}", name()),
        _ => format!("TABLE {}", name()),
    }
}

fn no_roles() -> Error {
    Error::Unsupported("RisingWave no tiene roles: los permisos se otorgan a cada usuario".into())
}

fn no_database() -> Error {
    Error::Query("en RisingWave los permisos sobre la base llevan su nombre: elegí el tipo «base» y cuál".into())
}

pub(super) fn script(a: &SecurityAction) -> Result<String> {
    let lit = |s: &str| crate::catalog::lit(crate::Variant::RisingWave, s);
    Ok(match a {
        SecurityAction::CreateUser { name, password } => match password.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("CREATE USER {} WITH LOGIN PASSWORD {};", q(name), lit(p)),
            None => format!("CREATE USER {} WITH LOGIN;", q(name)),
        },
        SecurityAction::CreateRole { .. } | SecurityAction::AddMember { .. } | SecurityAction::RemoveMember { .. } => return Err(no_roles()),
        SecurityAction::Drop { name, .. } => format!("DROP USER {};", q(name)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} WITH PASSWORD {};", q(name), lit(password)),
        SecurityAction::SetLogin { name, enabled } => {
            format!("ALTER USER {} WITH {};", q(name), if *enabled { "LOGIN" } else { "NOLOGIN" })
        }
        SecurityAction::Grant { object: None, .. } | SecurityAction::Revoke { object: None, .. } => return Err(no_database()),
        SecurityAction::Grant { privileges: p, object: Some(o), to, grantable } => format!(
            "GRANT {} ON {} TO {}{};",
            privileges(p)?,
            target(o),
            q(to),
            if *grantable { " WITH GRANT OPTION" } else { "" }
        ),
        SecurityAction::Revoke { privileges: p, object: Some(o), from } => {
            format!("REVOKE {} ON {} FROM {};", privileges(p)?, target(o), q(from))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: Some("pub\"lic".into()), name: "fac\"turas".into() })
    }

    #[test]
    fn scripts_quote_and_escape() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana\"x".into(), password: Some("p'w\\".into()) }),
            "CREATE USER \"ana\"\"x\" WITH LOGIN PASSWORD 'p''w\\';"
        );
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: None }), "CREATE USER \"ana\" WITH LOGIN;");
        assert_eq!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), "DROP USER \"ana\";");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "x'y".into() }), "ALTER USER \"ana\" WITH PASSWORD 'x''y';");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ALTER USER \"ana\" WITH NOLOGIN;");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: true }), "ALTER USER \"ana\" WITH LOGIN;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "update".into()], object: obj("table"), to: "ana".into(), grantable: true }),
            "GRANT SELECT, UPDATE ON TABLE \"pub\"\"lic\".\"fac\"\"turas\" TO \"ana\" WITH GRANT OPTION;"
        );
        // A named database, as the tab sends it (and as a listed grant's revoke).
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["connect".into(), "CREATE".into()],
                object: Some(ObjectRef { kind: "database".into(), schema: None, name: "d\"ev".into() }),
                to: "ana".into(),
                grantable: true
            }),
            "GRANT CONNECT, CREATE ON DATABASE \"d\"\"ev\" TO \"ana\" WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Revoke {
                privileges: vec!["CONNECT".into()],
                object: Some(ObjectRef { kind: "database".into(), schema: None, name: "dev".into() }),
                from: "ana".into()
            }),
            "REVOKE CONNECT ON DATABASE \"dev\" FROM \"ana\";"
        );
        for (kind, word) in [("materialized_view", "MATERIALIZED VIEW"), ("view", "VIEW"), ("source", "SOURCE"), ("sink", "SINK")] {
            assert_eq!(
                s(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: obj(kind), from: "ana".into() }),
                format!("REVOKE SELECT ON {word} \"pub\"\"lic\".\"fac\"\"turas\" FROM \"ana\";")
            );
        }
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["USAGE".into(), "create".into()],
                object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "ventas".into() }),
                to: "ana".into(),
                grantable: false
            }),
            "GRANT USAGE, CREATE ON SCHEMA \"ventas\" TO \"ana\";"
        );
    }

    #[test]
    fn spec_follows_the_explorer() {
        let s = super::super::spec(crate::Variant::RisingWave).unwrap();
        assert_eq!(s.object_kinds, vec!["database", "schema", "table", "view", "materialized_view", "source", "sink"]);
        assert!(s.privileges.contains(&"CONNECT"));
        assert!(!s.create_role && !s.membership && s.create_user && s.passwords);
        let h2 = super::super::spec(crate::Variant::H2).unwrap();
        assert!(h2.object_kinds.contains(&"") && h2.create_role && h2.membership);
    }

    #[test]
    fn no_roles_nor_database_grants() {
        assert!(script(&SecurityAction::CreateRole { name: "r".into() }).is_err());
        assert!(script(&SecurityAction::AddMember { role: "r".into(), member: "a".into() }).is_err());
        assert!(script(&SecurityAction::RemoveMember { role: "r".into(), member: "a".into() }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["CONNECT".into()], object: None, to: "a".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Revoke { privileges: vec!["CONNECT".into()], object: None, from: "a".into() }).is_err());
    }

    #[test]
    fn privilege_names_cannot_inject() {
        for bad in ["SELECT; DROP USER root", "SELECT --", "SEL'ECT", ""] {
            let a = SecurityAction::Grant { privileges: vec![bad.into()], object: obj("table"), to: "a".into(), grantable: false };
            assert!(script(&a).is_err(), "{bad}");
        }
    }

    #[test]
    fn acl_items() {
        let item = parse_acl_item("ana=r*wa/root", &["ana", "root"]).unwrap();
        assert_eq!(
            item,
            AclItem { grantee: "ana".into(), privileges: vec![("SELECT", true), ("UPDATE", false), ("INSERT", false)], grantor: "root".into() }
        );
        assert_eq!(parse_acl_item("ana=CU/root", &["ana", "root"]).unwrap().privileges, vec![("CREATE", false), ("USAGE", false)]);
        assert_eq!(parse_acl_item("ana=c*C/root", &["ana", "root"]).unwrap().privileges, vec![("CONNECT", true), ("CREATE", false)]);
        // Unquoted names: "a=b" is its own user, not "a".
        assert!(parse_acl_item("a=b=r/root", &["a", "root"]).is_none());
        assert_eq!(parse_acl_item("a=b=r/root", &["a", "a=b", "root"]).unwrap().grantee, "a=b");
        assert_eq!(parse_acl_item("odd\"n,a=me=r/ro/ot", &["odd\"n,a=me", "ro/ot"]).unwrap().grantor, "ro/ot");
        // "a=r/b"'s UPDATE isn't "a"'s SELECT: the grantor must be a user.
        let item = parse_acl_item("a=r/b=w/root", &["a", "a=r/b", "root"]).unwrap();
        assert_eq!((item.grantee.as_str(), item.privileges.as_slice()), ("a=r/b", &[("UPDATE", false)][..]));
        assert!(parse_acl_item("a=r/b=w/root", &["a", "root"]).is_none());
        assert!(parse_acl_item("anabel=r/root", &["ana", "root"]).is_none());
        assert!(parse_acl_item("ana=/root", &["ana", "root"]).is_none());
    }

    #[test]
    fn users_from_the_catalog() {
        let u = user("rwadmin".into(), |c| matches!(c, "can_login" | "is_super" | "is_admin"));
        assert_eq!((u.kind, u.can_login, u.superuser, u.disabled, u.system), (PrincipalKind::User, Some(true), Some(true), Some(false), true));
        assert!(u.details.contains(&("Administrador del clúster".to_string(), "sí".to_string())));
        let u = user("ana".into(), |_| false);
        assert_eq!((u.disabled, u.system, u.details.len()), (Some(true), false, 1));
    }
}

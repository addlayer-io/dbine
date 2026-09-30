//! StarRocks (3.x and later): users `'user'@'host'`, roles, and privileges
//! per object type (`GRANT SELECT ON TABLE db.t TO USER …`,
//! `… ON ALL TABLES IN DATABASE db`, `… ON SYSTEM`, `… ON CATALOG c`).
//!
//! Users are named `user@host` and roles `role:<name>`. Grants are read
//! from `SHOW GRANTS FOR` (users and roles), which keeps the grant's shape
//! (`ALL TABLES IN DATABASE db`) where `sys.grants_to_users` expands it;
//! role memberships from `sys.role_edges`. An object grant reads back as:
//! `db.t` (table, view…), `db.*` (all of a kind in a database), `*.*` (all
//! of a kind everywhere), `db` / `*` (a database / all), a catalog by name,
//! and `ON SYSTEM` as the whole server.

use super::common::{bare_role, privileges, q, role, unsupported, who, Who};
use super::{account, new_user, split_top, top_level, unquote};
use crate::session::{at, lit, named, MySqlSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::{HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "EXPORT", "ALTER", "DROP", "REFRESH", "USAGE", "CREATE TABLE", "CREATE VIEW",
            "CREATE MATERIALIZED VIEW", "CREATE FUNCTION", "CREATE DATABASE", "ALL", "OPERATE", "NODE", "GRANT", "CREATE RESOURCE GROUP",
            "CREATE EXTERNAL CATALOG", "PLUGIN", "FILE", "REPOSITORY", "BLACKLIST",
        ],
        // "" = the server (SYSTEM, and the default catalog for CREATE DATABASE).
        object_kinds: vec!["", "schema", "table", "view", "materialized_view"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

/// Built-in roles and their holders.
const BUILTIN_ROLES: [&str; 6] = ["root", "db_admin", "cluster_admin", "user_admin", "security_admin", "public"];

// -- reading -----------------------------------------------------------------

/// `'user'@'host'` from `SHOW ALL AUTHENTICATION` / `sys.role_edges` as
/// DBine names it: `user@host`.
pub(super) fn identity(s: &str) -> String {
    let parts = split_top(s, "@");
    match parts.as_slice() {
        [u, h] => format!("{}@{}", unquote(u), unquote(h)),
        _ => unquote(s),
    }
}

pub async fn principals(s: &mut MySqlSession) -> Result<Vec<Principal>> {
    let mut out: Vec<Principal> = Vec::new();
    let users = match s.rows("SHOW ALL AUTHENTICATION").await {
        Ok(r) => r.iter().map(|r| (named(r, &["UserIdentity"]).unwrap_or_default(), named(r, &["AuthPlugin"]))).collect::<Vec<_>>(),
        Err(_) => s.rows("SHOW USERS").await?.iter().map(|r| (at(r, 0).unwrap_or_default(), None)).collect(),
    };
    for (ident, plugin) in users {
        let name = identity(&ident);
        let user = name.rsplit_once('@').map_or(name.as_str(), |(u, _)| u).to_string();
        let mut details = vec![("Host".into(), name.rsplit_once('@').map_or("%", |(_, h)| h).to_string())];
        if let Some(p) = plugin.filter(|p| !p.is_empty()) {
            details.push(("Autenticación".into(), p));
        }
        out.push(Principal { name, kind: PrincipalKind::User, can_login: Some(true), details, system: user == "root", ..Default::default() });
    }
    for r in s.optional_rows("SHOW ROLES").await {
        let name = named(&r, &["Name"]).unwrap_or_default();
        let builtin = named(&r, &["Builtin"]).is_some_and(|b| b.eq_ignore_ascii_case("true")) || BUILTIN_ROLES.contains(&name.as_str());
        let mut details = Vec::new();
        if let Some(c) = named(&r, &["Comment"]).filter(|c| !c.is_empty()) {
            details.push(("Comentario".into(), c));
        }
        out.push(Principal {
            name: role(&name),
            kind: PrincipalKind::Role,
            superuser: Some(name == "root"),
            details,
            system: builtin,
            ..Default::default()
        });
    }
    // Memberships: FROM_ROLE is held by TO_ROLE or TO_USER.
    let edges: Vec<(String, String)> = match s.rows("SELECT FROM_ROLE, TO_ROLE, TO_USER FROM sys.role_edges").await {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| {
                let from = at(r, 0)?;
                let to = at(r, 1).filter(|t| !t.is_empty()).map(|t| role(&t)).or_else(|| at(r, 2).filter(|t| !t.is_empty()).map(|u| identity(&u)))?;
                Some((to, role(&from)))
            })
            .collect(),
        // Before sys.role_edges: each principal's `GRANT 'r' TO …` lines.
        Err(_) => {
            let names: Vec<String> = out.iter().map(|p| p.name.clone()).collect();
            let mut edges = Vec::new();
            for n in names {
                for line in show_grants(s, &n).await.unwrap_or_default() {
                    if let Some(Line::Roles(rs)) = parse_grant(&line) {
                        edges.extend(rs.into_iter().map(|r| (n.clone(), role(&r))));
                    }
                }
            }
            edges
        }
    };
    for (member, r) in edges {
        if let Some(p) = out.iter_mut().find(|p| p.name == member) {
            if !p.member_of.contains(&r) {
                p.member_of.push(r);
            }
        }
    }
    for p in out.iter_mut().filter(|p| p.kind == PrincipalKind::User) {
        p.superuser = Some(p.member_of.iter().any(|r| r == "role:root"));
    }
    Ok(out)
}

/// The `Grants` column of `SHOW GRANTS FOR` a user or `ROLE` a role.
async fn show_grants(s: &mut MySqlSession, name: &str) -> Result<Vec<String>> {
    let target = match who(name) {
        Who::Role(r) | Who::Group(r) => format!("ROLE {}", q(r)),
        Who::User(u) => account(u),
    };
    let rows = s.rows(&format!("SHOW GRANTS FOR {target}")).await?;
    Ok(rows.iter().filter_map(|r| named(r, &["Grants"]).or_else(|| at(r, 2))).collect())
}

pub async fn grants(s: &mut MySqlSession, principal: &str) -> Result<Vec<Grant>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::from([(principal.to_string(), None)]);
    while let Some((name, via)) = queue.pop_front() {
        if !seen.insert(name.clone()) || seen.len() > 64 {
            continue;
        }
        let lines = match show_grants(s, &name).await {
            Ok(l) => l,
            Err(e) if via.is_none() => return Err(e),
            Err(_) => continue,
        };
        for line in lines {
            match parse_grant(&line) {
                Some(Line::Privileges { privileges, objects, grantable }) => {
                    for (object, object_kind) in objects {
                        for p in &privileges {
                            out.push(Grant { privilege: p.clone(), object: object.clone(), object_kind: object_kind.clone(), grantable, denied: false, via: via.clone() });
                        }
                    }
                }
                Some(Line::Roles(roles)) => {
                    for r in roles {
                        let r = role(&r);
                        let v = via.clone().unwrap_or_else(|| r.clone());
                        queue.push_back((r, Some(v)));
                    }
                }
                None => {}
            }
        }
    }
    Ok(out)
}

// -- SHOW GRANTS parsing -------------------------------------------------------

#[derive(Debug, PartialEq)]
pub(super) enum Line {
    /// Privileges on objects: `(object, object kind)` each.
    Privileges { privileges: Vec<String>, objects: Vec<(Option<String>, Option<String>)>, grantable: bool },
    Roles(Vec<String>),
}

/// Object-type keywords, longest first, with DBine's kind id.
const TYPES: [(&str, &str); 14] = [
    ("MATERIALIZED VIEW", "materialized_view"),
    ("GLOBAL FUNCTION", "global_function"),
    ("RESOURCE GROUP", "resource_group"),
    ("STORAGE VOLUME", "storage_volume"),
    ("FUNCTION", "function"),
    ("DATABASE", "schema"),
    ("WAREHOUSE", "warehouse"),
    ("RESOURCE", "resource"),
    ("CATALOG", "catalog"),
    ("TABLE", "table"),
    ("VIEW", "view"),
    ("PIPE", "pipe"),
    ("USER", "user"),
    ("SYSTEM", ""),
];

/// `db.t` / `` `d b`.`t` `` without quotes, dotted.
fn dotted(s: &str) -> String {
    split_top(s, ".").into_iter().map(unquote).collect::<Vec<_>>().join(".")
}

/// The target of `ON …`: one or more objects.
fn parse_target(t: &str) -> Vec<(Option<String>, Option<String>)> {
    let t = t.trim();
    if let Some(rest) = t.strip_prefix("ALL ") {
        for (kw, kind) in TYPES {
            let Some(after) = rest.strip_prefix(kw).and_then(|a| a.strip_prefix('S')) else { continue };
            let after = after.trim();
            let object = if after.is_empty() {
                // ALL DATABASES / ALL CATALOGS / ALL RESOURCES…: every one.
                if matches!(kind, "schema" | "catalog" | "resource" | "resource_group" | "storage_volume" | "global_function" | "warehouse" | "user") {
                    "*".to_string()
                } else {
                    "*.*".to_string()
                }
            } else if after == "IN ALL DATABASES" {
                "*.*".to_string()
            } else if let Some(db) = after.strip_prefix("IN DATABASE ") {
                format!("{}.*", unquote(db))
            } else {
                continue;
            };
            return vec![(Some(object), Some(kind.to_string()))];
        }
        return vec![(Some(t.to_string()), None)];
    }
    if t == "SYSTEM" {
        return vec![(None, None)];
    }
    for (kw, kind) in TYPES {
        if let Some(rest) = t.strip_prefix(kw).and_then(|r| r.strip_prefix(' ')) {
            if kind == "user" {
                return vec![(Some(identity(rest)), Some(kind.into()))];
            }
            return split_top(rest, ",").into_iter().map(|o| (Some(dotted(o)), Some(kind.to_string()))).collect();
        }
    }
    vec![(Some(t.to_string()), None)]
}

pub(super) fn parse_grant(line: &str) -> Option<Line> {
    let body = line.trim().strip_prefix("GRANT ")?;
    let to = *top_level(body, " TO ").first()?;
    let on = top_level(body, " ON ").into_iter().find(|&i| i < to);
    let Some(on) = on else {
        return Some(Line::Roles(split_top(&body[..to], ",").into_iter().filter(|r| !r.is_empty()).map(unquote).collect()));
    };
    let privileges = split_top(&body[..on], ",").into_iter().filter(|p| !p.is_empty()).map(str::to_string).collect();
    let objects = parse_target(&body[on + 4..to]);
    Some(Line::Privileges { privileges, objects, grantable: body[to..].contains("WITH GRANT OPTION") })
}

// -- scripts -----------------------------------------------------------------

/// Privileges on a database's tables rather than on the database.
const TABLE_PRIVS: [&str; 6] = ["SELECT", "INSERT", "UPDATE", "DELETE", "EXPORT", "REFRESH"];

/// `TO USER 'u'@'h'` / `TO ROLE `r``.
fn grantee(name: &str) -> Result<String> {
    Ok(match who(name) {
        Who::Role(r) => format!("ROLE {}", q(r)),
        Who::User(u) => format!("USER {}", account(u)),
        Who::Group(_) => return Err(unsupported("StarRocks no tiene grupos")),
    })
}

/// A function's `name(ARG, TYPES)`: the name quoted, the argument types
/// checked (letters, digits, spaces, commas and parentheses, balanced, and
/// nothing after the closing one: `f(INT) TO ROLE x, (y)` is rejected).
fn function(name: &str) -> Result<String> {
    let signature = |args: &str| {
        let mut depth = 1;
        args.char_indices().all(|(i, c)| {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                c if !(c.is_ascii_alphanumeric() || " ,_".contains(c)) => return false,
                _ => {}
            }
            // Only the last character may close the list.
            depth > 0 || i == args.len() - 1
        }) && depth == 0
    };
    match name.split_once('(') {
        Some((n, args)) if signature(args) => Ok(format!("{}({args}", q(n))),
        Some(_) => Err(Error::Query(format!("«{name}» no es una firma de función válida"))),
        None => Ok(q(name)),
    }
}

/// The `ON …` targets for these privileges: the whole-server grant splits
/// into the default catalog (CREATE DATABASE, USAGE) and SYSTEM, a database
/// into its tables (SELECT, INSERT…) and the database itself.
fn targets(privs: &[String], object: &Option<ObjectRef>) -> Result<Vec<(Vec<String>, String)>> {
    let split = |pred: &dyn Fn(&str) -> bool, yes: String, no: String| {
        let (a, b): (Vec<String>, Vec<String>) = privs.iter().cloned().partition(|p| pred(p));
        [(a, yes), (b, no)].into_iter().filter(|(p, _)| !p.is_empty()).collect::<Vec<_>>()
    };
    let Some(o) = object else {
        return Ok(split(&|p| p == "CREATE DATABASE" || p == "USAGE", "CATALOG default_catalog".into(), "SYSTEM".into()));
    };
    let kind = o.kind.as_str();
    if kind == "schema" || kind == "database" {
        if o.name == "*" {
            return Ok(vec![(privs.to_vec(), "ALL DATABASES".into())]);
        }
        let db = q(&o.name);
        return Ok(split(&|p| TABLE_PRIVS.contains(&p), format!("ALL TABLES IN DATABASE {db}"), format!("DATABASE {db}")));
    }
    let Some((kw, _)) = TYPES.iter().find(|(_, k)| *k == kind && !k.is_empty()) else {
        return Err(Error::Query(format!("StarRocks no otorga permisos sobre «{kind}»")));
    };
    // Objects outside databases: the UI splits a read-back name at its first
    // dot (`ana@10.0.0.1` → schema `ana@10`), so the whole name is rejoined.
    let global = matches!(kind, "catalog" | "resource" | "resource_group" | "storage_volume" | "global_function" | "warehouse" | "user");
    let whole = o.schema().map_or_else(|| o.name.clone(), |s| format!("{s}.{}", o.name));
    let target = match (o.schema(), o.name.as_str()) {
        _ if global && whole != "*" => match kind {
            "user" => format!("USER {}", account(&whole)),
            "global_function" => format!("{kw} {}", function(&whole)?),
            _ => format!("{kw} {}", q(&whole)),
        },
        (Some("*"), "*") => format!("ALL {kw}S IN ALL DATABASES"),
        (None, "*") if global => format!("ALL {kw}S"),
        (Some(db), "*") => format!("ALL {kw}S IN DATABASE {}", q(db)),
        (Some(db), n) if kind == "function" => format!("FUNCTION {}.{}", q(db), function(n)?),
        (None, n) if kind == "global_function" || kind == "function" => format!("{kw} {}", function(n)?),
        (Some(db), n) => format!("{kw} {}.{}", q(db), q(n)),
        (None, n) => format!("{kw} {}", q(n)),
    };
    Ok(vec![(privs.to_vec(), target)])
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} IDENTIFIED BY {};", new_user(name), lit(pw))
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", q(bare_role(name))),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(bare_role(name))),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", account(name)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} IDENTIFIED BY {};", account(name), lit(password)),
        SecurityAction::SetLogin { .. } => return Err(unsupported("StarRocks no permite deshabilitar el ingreso de un usuario: cambiale la contraseña o borralo")),
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            let p = privileges(p, "StarRocks")?;
            let to = grantee(to)?;
            let opt = if *grantable { " WITH GRANT OPTION" } else { "" };
            targets(&p, object)?.into_iter().map(|(p, t)| format!("GRANT {} ON {t} TO {to}{opt};", p.join(", "))).collect::<Vec<_>>().join("\n")
        }
        SecurityAction::Revoke { privileges: p, object, from } => {
            let p = privileges(p, "StarRocks")?;
            let from = grantee(from)?;
            targets(&p, object)?.into_iter().map(|(p, t)| format!("REVOKE {} ON {t} FROM {from};", p.join(", "))).collect::<Vec<_>>().join("\n")
        }
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", q(bare_role(role)), grantee(member)?),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", q(bare_role(role)), grantee(member)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    fn grant(p: &[&str], object: Option<ObjectRef>, to: &str, grantable: bool) -> SecurityAction {
        SecurityAction::Grant { privileges: p.iter().map(|s| s.to_string()).collect(), object, to: to.into(), grantable }
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w".into()) }), "CREATE USER 'ana'@'%' IDENTIFIED BY 'p''w';");
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "lect`x".into() }), "CREATE ROLE `lect``x`;");
        assert_eq!(s(SecurityAction::CreateRole { name: "role:lect".into() }), "CREATE ROLE `lect`;");
        assert_eq!(s(SecurityAction::Drop { name: "role:lect".into(), kind: PrincipalKind::Role }), "DROP ROLE `lect`;");
        assert_eq!(s(SecurityAction::Drop { name: "ana@%".into(), kind: PrincipalKind::User }), "DROP USER 'ana'@'%';");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana@%".into(), password: "n".into() }), "ALTER USER 'ana'@'%' IDENTIFIED BY 'n';");
        assert!(matches!(script(&SecurityAction::SetLogin { name: "ana@%".into(), enabled: false }), Err(Error::Unsupported(_))));
        assert_eq!(
            s(grant(&["select", "UPDATE"], obj("table", Some("ventas"), "fac`t"), "ana@%", true)),
            "GRANT SELECT, UPDATE ON TABLE `ventas`.`fac``t` TO USER 'ana'@'%' WITH GRANT OPTION;"
        );
        assert_eq!(s(grant(&["SELECT"], obj("view", Some("v"), "w"), "role:lect", false)), "GRANT SELECT ON VIEW `v`.`w` TO ROLE `lect`;");
        assert_eq!(s(grant(&["REFRESH"], obj("materialized_view", Some("d"), "*"), "role:r", false)), "GRANT REFRESH ON ALL MATERIALIZED VIEWS IN DATABASE `d` TO ROLE `r`;");
        assert_eq!(
            s(grant(&["SELECT", "CREATE TABLE", "ALTER"], obj("schema", None, "ventas"), "role:r", false)),
            "GRANT SELECT ON ALL TABLES IN DATABASE `ventas` TO ROLE `r`;\nGRANT CREATE TABLE, ALTER ON DATABASE `ventas` TO ROLE `r`;"
        );
        assert_eq!(
            s(grant(&["OPERATE", "CREATE DATABASE"], None, "role:r", false)),
            "GRANT CREATE DATABASE ON CATALOG default_catalog TO ROLE `r`;\nGRANT OPERATE ON SYSTEM TO ROLE `r`;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: obj("table", Some("*"), "*"), from: "ana@%".into() }),
            "REVOKE SELECT ON ALL TABLES IN ALL DATABASES FROM USER 'ana'@'%';"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["USAGE".into()], object: obj("catalog", None, "hive"), from: "role:r".into() }),
            "REVOKE USAGE ON CATALOG `hive` FROM ROLE `r`;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["USAGE".into()], object: obj("function", Some("db"), "f(INT, VARCHAR(10))"), from: "role:r".into() }),
            "REVOKE USAGE ON FUNCTION `db`.`f`(INT, VARCHAR(10)) FROM ROLE `r`;"
        );
        assert_eq!(s(SecurityAction::Revoke { privileges: vec!["ALTER".into()], object: obj("schema", None, "*"), from: "role:r".into() }), "REVOKE ALTER ON ALL DATABASES FROM ROLE `r`;");
        assert_eq!(s(SecurityAction::AddMember { role: "role:lect".into(), member: "ana@%".into() }), "GRANT `lect` TO USER 'ana'@'%';");
        assert_eq!(s(SecurityAction::AddMember { role: "role:lect".into(), member: "role:sup".into() }), "GRANT `lect` TO ROLE `sup`;");
        assert_eq!(s(SecurityAction::RemoveMember { role: "role:lect".into(), member: "ana@%".into() }), "REVOKE `lect` FROM USER 'ana'@'%';");
        assert!(script(&grant(&["SELECT; DROP TABLE x"], None, "ana@%", false)).is_err());
        assert!(script(&grant(&["USAGE"], obj("function", Some("d"), "f(INT); DROP"), "ana@%", false)).is_err());
        assert!(script(&grant(&["SELECT"], obj("trigger", Some("d"), "t"), "ana@%", false)).is_err());
        // Balanced argument list, nothing after it.
        for bad in ["f(INT) TO ROLE root, (x)", "f(INT)) TO ROLE root", "f(INT", "f(INT) x"] {
            assert!(script(&grant(&["USAGE"], obj("function", Some("d"), bad), "ana@%", false)).is_err(), "{bad}");
        }
        assert_eq!(
            s(grant(&["USAGE"], obj("global_function", None, "g(DECIMAL(10, 2))"), "role:r", false)),
            "GRANT USAGE ON GLOBAL FUNCTION `g`(DECIMAL(10, 2)) TO ROLE `r`;"
        );
        // A read-back name the UI split at its first dot is rejoined.
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["IMPERSONATE".into()], object: obj("user", Some("ana@10"), "0.0.1"), from: "bob@%".into() }),
            "REVOKE IMPERSONATE ON USER 'ana'@'10.0.0.1' FROM USER 'bob'@'%';"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["USAGE".into()], object: obj("resource", Some("spark"), "r1"), from: "role:r".into() }),
            "REVOKE USAGE ON RESOURCE `spark.r1` FROM ROLE `r`;"
        );
        assert_eq!(s(SecurityAction::Revoke { privileges: vec!["USAGE".into()], object: obj("catalog", None, "*"), from: "role:r".into() }), "REVOKE USAGE ON ALL CATALOGS FROM ROLE `r`;");
    }

    #[test]
    fn parses_show_grants() {
        let p = |l: &str| parse_grant(l).unwrap();
        let privs = |l: &str| match p(l) {
            Line::Privileges { privileges, objects, grantable } => (privileges, objects, grantable),
            other => panic!("{other:?}"),
        };
        let o = |o: Option<&str>, k: Option<&str>| (o.map(String::from), k.map(String::from));
        assert_eq!(p("GRANT 'dbx_sup' TO 'dbx_ana'@'%'"), Line::Roles(vec!["dbx_sup".into()]));
        assert_eq!(p("GRANT 'a', 'b' TO ROLE dbx_sup"), Line::Roles(vec!["a".into(), "b".into()]));
        assert_eq!(
            privs("GRANT DELETE, UPDATE ON TABLE dbine_secx.f TO USER 'dbx_ana'@'%' WITH GRANT OPTION"),
            (vec!["DELETE".into(), "UPDATE".into()], vec![o(Some("dbine_secx.f"), Some("table"))], true)
        );
        assert_eq!(privs("GRANT SELECT ON ALL TABLES IN DATABASE dbine_secx TO USER 'dbx_ana'@'%'").1, vec![o(Some("dbine_secx.*"), Some("table"))]);
        assert_eq!(privs("GRANT CREATE TABLE, ALTER ON DATABASE dbine_secx TO USER 'dbx_ana'@'%'").1, vec![o(Some("dbine_secx"), Some("schema"))]);
        assert_eq!(privs("GRANT OPERATE ON SYSTEM TO USER 'dbx_ana'@'%'").1, vec![o(None, None)]);
        assert_eq!(privs("GRANT CREATE DATABASE ON CATALOG default_catalog TO USER 'x'@'%'").1, vec![o(Some("default_catalog"), Some("catalog"))]);
        assert_eq!(privs("GRANT DELETE, DROP ON ALL TABLES IN ALL DATABASES TO ROLE 'db_admin'").1, vec![o(Some("*.*"), Some("table"))]);
        assert_eq!(privs("GRANT ALTER, DROP ON ALL DATABASES TO ROLE 'db_admin'").1, vec![o(Some("*"), Some("schema"))]);
        assert_eq!(privs("GRANT USAGE, CREATE DATABASE ON ALL CATALOGS TO ROLE 'db_admin'").1, vec![o(Some("*"), Some("catalog"))]);
        assert_eq!(privs("GRANT ALTER, REFRESH ON ALL MATERIALIZED VIEWS IN ALL DATABASES TO ROLE 'db_admin'").1, vec![o(Some("*.*"), Some("materialized_view"))]);
        assert_eq!(privs("GRANT ALTER, DROP ON ALL RESOURCE GROUPS TO ROLE 'db_admin'").1, vec![o(Some("*"), Some("resource_group"))]);
        assert_eq!(privs("GRANT SELECT ON TABLE d.a, d.`b c` TO ROLE 'r'").1, vec![o(Some("d.a"), Some("table")), o(Some("d.b c"), Some("table"))]);
        assert_eq!(privs("GRANT IMPERSONATE ON USER 'x'@'%' TO USER 'y'@'%'").1, vec![o(Some("x@%"), Some("user"))]);
        assert_eq!(privs("GRANT SELECT ON TABLE `a TO b`.t TO ROLE 'r'").1, vec![o(Some("a TO b.t"), Some("table"))]);
    }

    #[test]
    fn reads_identities() {
        assert_eq!(identity("'dbx_ana'@'%'"), "dbx_ana@%");
        assert_eq!(identity("'a@b'@'10.0.0.1'"), "a@b@10.0.0.1");
        assert_eq!(identity("root"), "root");
    }
}

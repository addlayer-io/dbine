//! SingleStore: MySQL-style users (`'user'@'host'`) and grants, plus its
//! role-based access: privileges go to roles (`GRANT … TO ROLE 'r'`),
//! roles to groups (`GRANT ROLE 'r' TO 'g'`) and groups to users
//! (`GRANT GROUP 'g' TO 'u'@'h'`).
//!
//! Users are named `user@host`, roles `role:<name>` and groups
//! `group:<name>` (both listed as roles). Accounts, roles, groups and the
//! memberships come from information_schema (USERS, ROLES, GROUPS,
//! USERS_GROUPS, GROUPS_ROLES); grants from `SHOW GRANTS FOR`, following
//! user → group → role.

use super::common::{bare_role, privileges, role, unsupported, who, Who, GROUP};
use super::starrocks::identity;
use super::{account, new_user, on, parse_grant, Line};
use crate::session::{at, lit, named, MySqlSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::{HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "EXECUTE", "CREATE VIEW", "SHOW VIEW", "ALTER VIEW",
            "DROP VIEW", "CREATE ROUTINE", "ALTER ROUTINE", "CREATE TEMPORARY TABLES", "LOCK TABLES", "CREATE PIPELINE", "DROP PIPELINE",
            "ALTER PIPELINE", "START PIPELINE", "SHOW PIPELINE", "SHOW METADATA", "CREATE DATABASE", "DROP DATABASE", "BACKUP", "PROCESS",
            "RELOAD", "CREATE USER", "ALTER USER", "ALL PRIVILEGES",
        ],
        // "" = every database (`*.*`), "schema" = one database (`db.*`).
        object_kinds: vec!["", "schema", "table", "view"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

fn group(name: &str) -> String {
    format!("{GROUP}{name}")
}

// -- reading -----------------------------------------------------------------

/// A user as a view reports it: `'u'@'h'`, `u@h` or just `u`.
fn find_user(out: &[Principal], raw: &str) -> Option<usize> {
    let name = identity(raw);
    out.iter().position(|p| p.kind == PrincipalKind::User && p.name == name).or_else(|| {
        (!name.contains('@')).then(|| out.iter().position(|p| p.kind == PrincipalKind::User && p.name.rsplit_once('@').is_some_and(|(u, _)| u == name))).flatten()
    })
}

fn add(p: &mut Principal, m: String) {
    if !p.member_of.contains(&m) {
        p.member_of.push(m);
    }
}

pub async fn principals(s: &mut MySqlSession) -> Result<Vec<Principal>> {
    let mut out: Vec<Principal> = Vec::new();
    match s.rows("SELECT * FROM information_schema.USERS").await {
        Ok(rows) => {
            for r in rows {
                let user = named(&r, &["USER"]).unwrap_or_default();
                let host = named(&r, &["HOST"]).unwrap_or_else(|| "%".into());
                let mut details = vec![("Host".into(), host.clone())];
                for (col, label) in [("TYPE", "Autenticación"), ("DEFAULT_RESOURCE_POOL", "Pool de recursos"), ("CREATED", "Alta"), ("IS_LOCAL", "Alcance")] {
                    if let Some(v) = named(&r, &[col]).filter(|v| !v.is_empty()) {
                        details.push((label.into(), v));
                    }
                }
                let status = named(&r, &["ACCOUNT_STATUS"]).unwrap_or_default();
                out.push(Principal {
                    name: format!("{user}@{host}"),
                    kind: PrincipalKind::User,
                    can_login: Some(true),
                    disabled: Some(status.eq_ignore_ascii_case("LOCKED")),
                    system: user == "root",
                    details,
                    ..Default::default()
                });
            }
        }
        // Older releases: SHOW USERS (`'u'@'h'`, Type…).
        Err(_) => {
            for r in s.rows("SHOW USERS").await? {
                let name = identity(&at(&r, 0).unwrap_or_default());
                let system = name.starts_with("root@");
                out.push(Principal { name, kind: PrincipalKind::User, can_login: Some(true), system, ..Default::default() });
            }
        }
    }
    let roles = match s.rows("SELECT * FROM information_schema.ROLES").await {
        Ok(r) => r.iter().filter_map(|r| named(r, &["ROLE"])).collect::<Vec<_>>(),
        Err(_) => s.optional_rows("SHOW ROLES").await.iter().filter_map(|r| at(r, 0)).collect(),
    };
    out.extend(roles.into_iter().map(|r| Principal { name: role(&r), kind: PrincipalKind::Role, ..Default::default() }));
    let groups = match s.rows("SELECT * FROM information_schema.`GROUPS`").await {
        Ok(r) => r.iter().filter_map(|r| named(r, &["GROUP_NAME", "GROUP"])).collect::<Vec<_>>(),
        Err(_) => s.optional_rows("SHOW GROUPS").await.iter().filter_map(|r| at(r, 0)).collect(),
    };
    for g in groups {
        out.push(Principal { name: group(&g), kind: PrincipalKind::Role, details: vec![("Tipo".into(), "grupo".into())], ..Default::default() });
    }
    for r in s.optional_rows("SELECT * FROM information_schema.USERS_GROUPS").await {
        let (Some(g), Some(u)) = (named(&r, &["GROUP"]), named(&r, &["USER"])) else { continue };
        if let Some(i) = find_user(&out, &u) {
            add(&mut out[i], group(&g));
        }
    }
    for r in s.optional_rows("SELECT * FROM information_schema.GROUPS_ROLES").await {
        let (Some(g), Some(rl)) = (named(&r, &["GROUP"]), named(&r, &["ROLE"])) else { continue };
        let g = group(&g);
        if let Some(p) = out.iter_mut().find(|p| p.name == g) {
            add(p, role(&rl));
        }
    }
    // SUPER (or everything) on *.*: a superuser.
    let supers: Vec<String> = s
        .optional_rows("SELECT GRANTEE FROM information_schema.USER_PRIVILEGES WHERE PRIVILEGE_TYPE IN ('SUPER', 'ALL PRIVILEGES')")
        .await
        .iter()
        .filter_map(|r| at(r, 0))
        .map(|g| identity(&g))
        .collect();
    for p in out.iter_mut().filter(|p| p.kind == PrincipalKind::User) {
        p.superuser = Some(supers.contains(&p.name));
    }
    Ok(out)
}

/// A user's or role's own grants (groups hold none).
async fn own(s: &mut MySqlSession, name: &str, via: &Option<String>, out: &mut Vec<Grant>) -> Result<()> {
    let target = match who(name) {
        Who::User(u) => account(u),
        Who::Role(r) => format!("ROLE {}", lit(r)),
        Who::Group(_) => return Ok(()),
    };
    for r in s.rows(&format!("SHOW GRANTS FOR {target}")).await? {
        let line = at(&r, 0).unwrap_or_default();
        // TRANSFERABLE: SingleStore's per-grant grant option.
        let transferable = line.contains("GRANT TRANSFERABLE ");
        let line = line.replacen("GRANT TRANSFERABLE ", "GRANT ", 1);
        if let Some(Line::Privileges { privileges, object, object_kind, grantable }) = parse_grant(&line) {
            for p in privileges.into_iter().filter(|p| p != "USAGE") {
                out.push(Grant { privilege: p, object: object.clone(), object_kind: object_kind.clone(), grantable: grantable || transferable, denied: false, via: via.clone() });
            }
        }
    }
    Ok(())
}

pub async fn grants(s: &mut MySqlSession, principal: &str) -> Result<Vec<Grant>> {
    let all = principals(s).await?;
    let mut out = Vec::new();
    own(s, principal, &None, &mut out).await?;
    let member_of = |n: &str| all.iter().find(|p| p.name == n).map(|p| p.member_of.clone()).unwrap_or_default();
    let mut seen = HashSet::from([principal.to_string()]);
    let mut queue: VecDeque<(String, String)> = member_of(principal).into_iter().map(|m| (m.clone(), m)).collect();
    while let Some((name, via)) = queue.pop_front() {
        if !seen.insert(name.clone()) || seen.len() > 64 {
            continue;
        }
        if let Err(e) = own(s, &name, &Some(via.clone()), &mut out).await {
            tracing::debug!("SHOW GRANTS FOR {name}: {e}");
        }
        queue.extend(member_of(&name).into_iter().map(|m| (m, via.clone())));
    }
    Ok(out)
}

// -- scripts -----------------------------------------------------------------

/// `'u'@'h'` or `ROLE 'r'`; groups hold no privileges.
fn grantee(name: &str) -> Result<String> {
    Ok(match who(name) {
        Who::User(u) => account(u),
        Who::Role(r) => format!("ROLE {}", lit(r)),
        Who::Group(_) => return Err(unsupported("En SingleStore los grupos no reciben permisos: otorgáselos a un rol y dale el rol al grupo")),
    })
}

fn target(object: &Option<ObjectRef>) -> Result<String> {
    match object {
        Some(o) if !matches!(o.kind.as_str(), "schema" | "database" | "table" | "view") => Err(Error::Query(format!("SingleStore no otorga permisos sobre «{}»", o.kind))),
        _ => Ok(on(object)),
    }
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} IDENTIFIED BY {};", new_user(name), lit(pw))
        }
        SecurityAction::CreateRole { name } => match who(name) {
            Who::Group(g) => format!("CREATE GROUP {};", lit(g)),
            _ => format!("CREATE ROLE {};", lit(bare_role(name))),
        },
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => match who(name) {
            Who::Group(g) => format!("DROP GROUP {};", lit(g)),
            _ => format!("DROP ROLE {};", lit(bare_role(name))),
        },
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", account(name)),
        SecurityAction::SetPassword { name, password } => format!("SET PASSWORD FOR {} = PASSWORD({});", account(name), lit(password)),
        SecurityAction::SetLogin { name, enabled } => format!("ALTER USER {} ACCOUNT {};", account(name), if *enabled { "UNLOCK" } else { "LOCK" }),
        SecurityAction::Grant { privileges: p, object, to, grantable } => format!(
            "GRANT {} ON {} TO {}{};",
            privileges(p, "SingleStore")?.join(", "),
            target(object)?,
            grantee(to)?,
            if *grantable { " WITH GRANT OPTION" } else { "" }
        ),
        SecurityAction::Revoke { privileges: p, object, from } => {
            format!("REVOKE {} ON {} FROM {};", privileges(p, "SingleStore")?.join(", "), target(object)?, grantee(from)?)
        }
        SecurityAction::AddMember { role, member } => match (who(role), who(member)) {
            (Who::Group(g), Who::User(u)) => format!("GRANT GROUP {} TO {};", lit(g), account(u)),
            (Who::Role(r) | Who::User(r), Who::Group(g)) => format!("GRANT ROLE {} TO {};", lit(r), lit(g)),
            _ => return Err(membership()),
        },
        SecurityAction::RemoveMember { role, member } => match (who(role), who(member)) {
            (Who::Group(g), Who::User(u)) => format!("REVOKE GROUP {} FROM {};", lit(g), account(u)),
            (Who::Role(r) | Who::User(r), Who::Group(g)) => format!("REVOKE ROLE {} FROM {};", lit(r), lit(g)),
            _ => return Err(membership()),
        },
    })
}

fn membership() -> Error {
    unsupported("En SingleStore los usuarios entran a grupos y los grupos reciben roles: agregá el usuario a un grupo que tenga el rol")
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
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w".into()) }), "CREATE USER 'ana'@'%' IDENTIFIED BY 'p''w';");
        assert_eq!(s(SecurityAction::CreateRole { name: "lect".into() }), "CREATE ROLE 'lect';");
        assert_eq!(s(SecurityAction::CreateRole { name: "role:lect".into() }), "CREATE ROLE 'lect';");
        assert_eq!(s(SecurityAction::CreateRole { name: "group:ventas".into() }), "CREATE GROUP 'ventas';");
        assert_eq!(s(SecurityAction::Drop { name: "group:ventas".into(), kind: PrincipalKind::Role }), "DROP GROUP 'ventas';");
        assert_eq!(s(SecurityAction::Drop { name: "role:lect".into(), kind: PrincipalKind::Role }), "DROP ROLE 'lect';");
        assert_eq!(s(SecurityAction::Drop { name: "ana@%".into(), kind: PrincipalKind::User }), "DROP USER 'ana'@'%';");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana@%".into(), password: "n'\\".into() }), "SET PASSWORD FOR 'ana'@'%' = PASSWORD('n''\\\\');");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana@%".into(), enabled: false }), "ALTER USER 'ana'@'%' ACCOUNT LOCK;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into()], object: obj("table", Some("ventas"), "fac`t"), to: "ana@%".into(), grantable: true }),
            "GRANT SELECT ON `ventas`.`fac``t` TO 'ana'@'%' WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["SELECT".into(), "CREATE VIEW".into()], object: obj("schema", None, "ventas"), to: "role:lect".into(), grantable: false }),
            "GRANT SELECT, CREATE VIEW ON `ventas`.* TO ROLE 'lect';"
        );
        assert_eq!(s(SecurityAction::Revoke { privileges: vec!["PROCESS".into()], object: None, from: "role:lect".into() }), "REVOKE PROCESS ON *.* FROM ROLE 'lect';");
        assert!(matches!(
            script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: None, to: "group:g".into(), grantable: false }),
            Err(Error::Unsupported(_))
        ));
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT'".into()], object: None, to: "ana@%".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["EXECUTE".into()], object: obj("trigger", None, "t"), to: "ana@%".into(), grantable: false }).is_err());
        assert_eq!(s(SecurityAction::AddMember { role: "group:ventas".into(), member: "ana@%".into() }), "GRANT GROUP 'ventas' TO 'ana'@'%';");
        assert_eq!(s(SecurityAction::AddMember { role: "role:lect".into(), member: "group:ventas".into() }), "GRANT ROLE 'lect' TO 'ventas';");
        assert_eq!(s(SecurityAction::RemoveMember { role: "group:ventas".into(), member: "ana@%".into() }), "REVOKE GROUP 'ventas' FROM 'ana'@'%';");
        assert_eq!(s(SecurityAction::RemoveMember { role: "role:lect".into(), member: "group:ventas".into() }), "REVOKE ROLE 'lect' FROM 'ventas';");
        assert!(matches!(script(&SecurityAction::AddMember { role: "role:lect".into(), member: "ana@%".into() }), Err(Error::Unsupported(_))));
    }

    #[test]
    fn parses_role_grants() {
        let line = "GRANT TRANSFERABLE SELECT, INSERT, UPDATE ON `trades`.`company` TO ROLE 'rw'".replacen("GRANT TRANSFERABLE ", "GRANT ", 1);
        assert_eq!(
            parse_grant(&line),
            Some(Line::Privileges {
                privileges: vec!["SELECT".into(), "INSERT".into(), "UPDATE".into()],
                object: Some("trades.company".into()),
                object_kind: Some("table".into()),
                grantable: false
            })
        );
        let users = vec![
            Principal { name: "ana@%".into(), ..Default::default() },
            Principal { name: "role:ana".into(), kind: PrincipalKind::Role, ..Default::default() },
        ];
        assert_eq!(find_user(&users, "'ana'@'%'"), Some(0));
        assert_eq!(find_user(&users, "ana@%"), Some(0));
        assert_eq!(find_user(&users, "ana"), Some(0));
        assert_eq!(find_user(&users, "bob"), None);
    }
}

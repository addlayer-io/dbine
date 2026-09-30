//! Databend: users (`CREATE USER 'u' IDENTIFIED BY …`, disabled with
//! `ALTER USER … WITH DISABLED = true`), roles that hold roles, and
//! `GRANT … ON db.t TO 'u'` / `TO ROLE 'r'` (no WITH GRANT OPTION).
//!
//! Users are named as they are (Databend dropped hosts: every user is
//! `'u'@'%'`), roles `role:<name>`. `SHOW GRANTS FOR` returns what a
//! principal holds including through its roles, so a grant is direct when
//! none of the principal's roles (`SHOW USERS`' roles, `SHOW ROLES`'
//! inherited_roles_name) has it.

use super::common::{bare_role, privileges, q, role, unsupported, who, Who};
use super::{split_top, top_level, unquote};
use crate::session::{lit, named, MySqlSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashSet;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "DROP", "CREATE", "CREATE DATABASE", "SUPER", "CREATE USER", "CREATE ROLE",
            "GRANT", "USAGE", "READ", "WRITE", "OWNERSHIP", "ALL",
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

// -- reading -----------------------------------------------------------------

fn list(s: Option<String>) -> Vec<String> {
    s.unwrap_or_default().split(',').map(str::trim).filter(|r| !r.is_empty()).map(str::to_string).collect()
}

fn yes(v: Option<String>) -> bool {
    matches!(v.as_deref().map(|s| s.trim().to_ascii_lowercase()).as_deref(), Some("1" | "true" | "yes"))
}

pub async fn principals(s: &mut MySqlSession) -> Result<Vec<Principal>> {
    let mut out = Vec::new();
    for r in s.rows("SHOW USERS").await? {
        let name = named(&r, &["name"]).unwrap_or_default();
        let roles = list(named(&r, &["roles"]));
        let mut details = Vec::new();
        if let Some(h) = named(&r, &["hostname"]).filter(|h| !h.is_empty() && h != "%") {
            details.push(("Host".into(), h));
        }
        if let Some(a) = named(&r, &["auth_type"]).filter(|a| !a.is_empty()) {
            details.push(("Autenticación".into(), a));
        }
        if let Some(d) = named(&r, &["default_role"]).filter(|d| !d.is_empty()) {
            details.push(("Rol predeterminado".into(), d));
        }
        // Users from the server's configuration can't be changed with SQL.
        let configured = named(&r, &["is_configured"]).is_some_and(|c| c.eq_ignore_ascii_case("YES"));
        if configured {
            details.push(("Origen".into(), "configuración del servidor".into()));
        }
        out.push(Principal {
            superuser: Some(roles.iter().any(|r| r == "account_admin")),
            member_of: roles.iter().map(|r| role(r)).collect(),
            disabled: Some(yes(named(&r, &["disabled"]))),
            can_login: Some(true),
            kind: PrincipalKind::User,
            system: configured || name == "root",
            details,
            name,
        });
    }
    for r in s.optional_rows("SHOW ROLES").await {
        let name = named(&r, &["name"]).unwrap_or_default();
        out.push(Principal {
            name: role(&name),
            kind: PrincipalKind::Role,
            superuser: Some(name == "account_admin"),
            member_of: list(named(&r, &["inherited_roles_name"])).iter().map(|r| role(r)).collect(),
            system: name == "account_admin" || name == "public",
            ..Default::default()
        });
    }
    Ok(out)
}

/// A user as `'u'`, a role as `ROLE 'r'`.
fn grantee(name: &str) -> Result<String> {
    Ok(match who(name) {
        Who::Role(r) => format!("ROLE {}", lit(r)),
        Who::User(u) => lit(u),
        Who::Group(_) => return Err(unsupported("Databend no tiene grupos")),
    })
}

/// Each `(privilege, object, object kind)` in `SHOW GRANTS FOR`.
async fn effective(s: &mut MySqlSession, name: &str) -> Result<Vec<(String, Option<String>, Option<String>)>> {
    let rows = s.rows(&format!("SHOW GRANTS FOR {}", grantee(name)?)).await?;
    let mut out = Vec::new();
    for r in rows {
        let line = named(&r, &["grants", "Grants"]).or_else(|| crate::session::at(&r, 0)).unwrap_or_default();
        if let Some((privs, object, kind)) = parse_grant(&line) {
            out.extend(privs.into_iter().map(|p| (p, object.clone(), kind.clone())));
        }
    }
    Ok(out)
}

pub async fn grants(s: &mut MySqlSession, principal: &str) -> Result<Vec<Grant>> {
    let all = effective(s, principal).await?;
    // The roles it holds directly.
    let held: Vec<String> = match who(principal) {
        Who::Role(r) => s
            .optional_rows("SHOW ROLES")
            .await
            .iter()
            .find(|row| named(row, &["name"]).as_deref() == Some(r))
            .map(|row| list(named(row, &["inherited_roles_name"])))
            .unwrap_or_default(),
        _ => s
            .rows("SHOW USERS")
            .await?
            .iter()
            .find(|row| named(row, &["name"]).as_deref() == Some(principal))
            .map(|row| list(named(row, &["roles"])))
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    let mut through = HashSet::new();
    for r in held {
        let name = role(&r);
        for (p, object, kind) in effective(s, &name).await.unwrap_or_default() {
            if through.insert((p.clone(), object.clone())) {
                out.push(Grant { privilege: p, object, object_kind: kind, via: Some(name.clone()), ..Default::default() });
            }
        }
    }
    let direct = all.into_iter().filter(|(p, o, _)| !through.contains(&(p.clone(), o.clone())));
    let mut direct: Vec<Grant> = direct.map(|(p, object, kind)| Grant { privilege: p, object, object_kind: kind, ..Default::default() }).collect();
    direct.append(&mut out);
    Ok(direct)
}

/// `GRANT SELECT,INSERT ON 'default'.'db'.* TO …`: the privileges, the
/// object and its kind. `*.*` is everything; `db` a database ("schema").
pub(super) fn parse_grant(line: &str) -> Option<(Vec<String>, Option<String>, Option<String>)> {
    let body = line.trim().strip_prefix("GRANT ")?;
    let to = *top_level(body, " TO ").first()?;
    let on = top_level(body, " ON ").into_iter().find(|&i| i < to)?;
    let privs = split_top(&body[..on], ",").into_iter().filter(|p| !p.is_empty()).map(|p| p.to_uppercase()).collect();
    let target = body[on + 4..to].trim();
    let (object, kind) = match target.split_once(' ') {
        // STAGE s, UDF f, WAREHOUSE w, CONNECTION c, SEQUENCE s…
        Some((kw, rest)) if kw.chars().all(|c| c.is_ascii_uppercase()) => (Some(unquote(rest)), Some(kw.to_ascii_lowercase())),
        _ => {
            let parts: Vec<String> = split_top(target, ".").into_iter().map(unquote).collect();
            let parts = match parts.as_slice() {
                // catalog.db.x: the default catalog goes unsaid.
                [c, db, t] if c == "default" => vec![db.clone(), t.clone()],
                _ => parts,
            };
            match parts.as_slice() {
                [a, b] if a == "*" && b == "*" => (None, None),
                [db, t] if t == "*" => (Some(db.clone()), Some("schema".into())),
                [db, t] => (Some(format!("{db}.{t}")), Some("table".into())),
                _ => (Some(parts.join(".")), None),
            }
        }
    };
    Some((privs, object, kind))
}

// -- scripts -----------------------------------------------------------------

fn on(object: &Option<ObjectRef>) -> Result<String> {
    Ok(match object {
        None => "*.*".into(),
        Some(o) if o.kind == "schema" || o.kind == "database" => format!("{}.*", q(&o.name)),
        Some(o) if matches!(o.kind.as_str(), "table" | "view") => match o.schema() {
            Some(db) => format!("{}.{}", q(db), q(&o.name)),
            None => q(&o.name),
        },
        Some(o) if matches!(o.kind.as_str(), "stage" | "udf" | "warehouse" | "connection" | "sequence" | "procedure") => {
            format!("{} {}", o.kind.to_uppercase(), q(&o.name))
        }
        Some(o) => return Err(Error::Query(format!("Databend no otorga permisos sobre «{}»", o.kind))),
    })
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} IDENTIFIED BY {};", lit(name), lit(pw))
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", lit(bare_role(name))),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", lit(bare_role(name))),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", lit(name)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} IDENTIFIED BY {};", lit(name), lit(password)),
        SecurityAction::SetLogin { name, enabled } => format!("ALTER USER {} WITH DISABLED = {};", lit(name), !enabled),
        SecurityAction::Grant { grantable: true, .. } => {
            return Err(unsupported("Databend no tiene WITH GRANT OPTION: para que pueda otorgar, dale el permiso GRANT"))
        }
        SecurityAction::Grant { privileges: p, object, to, .. } => {
            format!("GRANT {} ON {} TO {};", privileges(p, "Databend")?.join(", "), on(object)?, grantee(to)?)
        }
        SecurityAction::Revoke { privileges: p, object, from } => {
            format!("REVOKE {} ON {} FROM {};", privileges(p, "Databend")?.join(", "), on(object)?, grantee(from)?)
        }
        SecurityAction::AddMember { role, member } => format!("GRANT ROLE {} TO {};", lit(bare_role(role)), grantee(member)?),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE ROLE {} FROM {};", lit(bare_role(role)), grantee(member)?),
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
        let s = |a| script(&a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w".into()) }), "CREATE USER 'ana' IDENTIFIED BY 'p''w';");
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: Some(String::new()) }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "role:lect".into() }), "CREATE ROLE 'lect';");
        assert_eq!(s(SecurityAction::Drop { name: "role:lect".into(), kind: PrincipalKind::Role }), "DROP ROLE 'lect';");
        assert_eq!(s(SecurityAction::Drop { name: "a'na".into(), kind: PrincipalKind::User }), "DROP USER 'a''na';");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "n".into() }), "ALTER USER 'ana' IDENTIFIED BY 'n';");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ALTER USER 'ana' WITH DISABLED = true;");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: true }), "ALTER USER 'ana' WITH DISABLED = false;");
        let grant = |p: &str, object, to: &str| SecurityAction::Grant { privileges: vec![p.into()], object, to: to.into(), grantable: false };
        assert_eq!(s(grant("select", obj("table", Some("ventas"), "fac`t"), "ana")), "GRANT SELECT ON `ventas`.`fac``t` TO 'ana';");
        assert_eq!(s(grant("INSERT", obj("schema", None, "ventas"), "role:lect")), "GRANT INSERT ON `ventas`.* TO ROLE 'lect';");
        assert_eq!(s(grant("CREATE DATABASE", None, "role:lect")), "GRANT CREATE DATABASE ON *.* TO ROLE 'lect';");
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["READ".into()], object: obj("stage", None, "st"), from: "ana".into() }),
            "REVOKE READ ON STAGE `st` FROM 'ana';"
        );
        assert!(matches!(
            script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: None, to: "ana".into(), grantable: true }),
            Err(Error::Unsupported(_))
        ));
        assert!(script(&grant("SELECT;DROP", None, "ana")).is_err());
        assert!(script(&grant("SELECT", obj("trigger", None, "t"), "ana")).is_err());
        assert_eq!(s(SecurityAction::AddMember { role: "role:lect".into(), member: "ana".into() }), "GRANT ROLE 'lect' TO 'ana';");
        assert_eq!(s(SecurityAction::AddMember { role: "role:lect".into(), member: "role:sup".into() }), "GRANT ROLE 'lect' TO ROLE 'sup';");
        assert_eq!(s(SecurityAction::RemoveMember { role: "role:lect".into(), member: "ana".into() }), "REVOKE ROLE 'lect' FROM 'ana';");
    }

    #[test]
    fn parses_show_grants() {
        let p = |l: &str| parse_grant(l).unwrap();
        let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(p("GRANT CREATE DATABASE ON *.* TO 'dbx_ana'@'%'"), (v(&["CREATE DATABASE"]), None, None));
        assert_eq!(p("GRANT SELECT,INSERT ON 'default'.'dbine_secx'.* TO 'dbx_ana'@'%'"), (v(&["SELECT", "INSERT"]), Some("dbine_secx".into()), Some("schema".into())));
        assert_eq!(
            p("GRANT SELECT,UPDATE,DELETE ON 'default'.'dbine_secx'.'f' TO 'dbx_ana'@'%'"),
            (v(&["SELECT", "UPDATE", "DELETE"]), Some("dbine_secx.f".into()), Some("table".into()))
        );
        assert_eq!(p("GRANT ALL ON *.* TO ROLE `account_admin`"), (v(&["ALL"]), None, None));
        assert_eq!(p("GRANT Read ON STAGE dbx_st TO 'dbx_ana'@'%'"), (v(&["READ"]), Some("dbx_st".into()), Some("stage".into())));
        assert_eq!(p("GRANT SELECT ON 'default'.'a TO b'.'t' TO 'u'@'%'"), (v(&["SELECT"]), Some("a TO b.t".into()), Some("table".into())));
        assert!(parse_grant("something else").is_none());
    }
}

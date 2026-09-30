//! Users, roles and permissions of Apache IoTDB / TimechoDB 1.x and 2.x
//! (tree model; docs/usuarios-y-permisos.md).
//!
//! `LIST USER` / `LIST ROLE` name them, `LIST USER OF ROLE` gives the
//! members, and `LIST PRIVILEGES OF USER|ROLE` the privileges with their
//! path pattern (empty for the global ones: MANAGE_USER, USE_UDF…) and,
//! for a user, the role each came through. `root` has every privilege.
//!
//! IoTDB's statements say whether the grantee is a user or a role
//! (`TO USER u`, `TO ROLE r`), so DBine names roles `role:<name>` and the
//! scripts read the kind back from the name.

use crate::{database_path, node, split_path, IotDbSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::Value as J;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "READ_DATA", "WRITE_DATA", "READ_SCHEMA", "WRITE_SCHEMA", "ALL", "MANAGE_DATABASE", "MANAGE_USER", "MANAGE_ROLE",
            "USE_TRIGGER", "USE_UDF", "USE_CQ", "USE_PIPE", "USE_MODEL", "EXTEND_TEMPLATE", "MAINTAIN",
        ],
        // "" = every path (`root.**`); "database" = a database's subtree.
        object_kinds: vec!["", "database"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

const ROLE_PREFIX: &str = "role:";

/// A role's name in DBine.
fn role_name(r: &str) -> String {
    format!("{ROLE_PREFIX}{r}")
}

/// A principal's name as `(is role, name in IoTDB)`.
fn grantee(name: &str) -> (bool, &str) {
    match name.strip_prefix(ROLE_PREFIX) {
        Some(r) => (true, r),
        None => (false, name),
    }
}

/// A user or role name: backquoted unless it's a plain name.
fn ident(name: &str) -> String {
    let plain = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.chars().all(|c| c.is_ascii_digit());
    if plain {
        name.to_string()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn text(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        J::Null => String::new(),
        v => v.to_string(),
    }
}

// -- reading -------------------------------------------------------------------

async fn names(s: &IotDbSession, sql: &str) -> Result<Vec<String>> {
    let t = s.query(sql, 100_000).await?;
    Ok(t.rows.iter().filter_map(|r| r.first().map(text)).filter(|n| !n.is_empty()).collect())
}

pub async fn principals(s: &IotDbSession) -> Result<Vec<Principal>> {
    let users = names(s, "LIST USER").await?;
    // Listing roles needs MANAGE_ROLE: without it, just the users.
    let roles = names(s, "LIST ROLE").await.unwrap_or_default();
    let mut out: Vec<Principal> = users
        .into_iter()
        .map(|u| Principal {
            superuser: Some(u == "root"),
            system: u == "root",
            can_login: Some(true),
            name: u,
            kind: PrincipalKind::User,
            ..Default::default()
        })
        .collect();
    for r in roles {
        for m in names(s, &format!("LIST USER OF ROLE {}", ident(&r))).await.unwrap_or_default() {
            if let Some(p) = out.iter_mut().find(|p| p.kind == PrincipalKind::User && p.name == m) {
                p.member_of.push(role_name(&r));
            }
        }
        out.push(Principal { name: role_name(&r), kind: PrincipalKind::Role, can_login: Some(false), ..Default::default() });
    }
    Ok(out)
}

/// A privilege's path as the grant's object: `None` for every path.
fn scope(path: &str) -> (Option<String>, Option<String>) {
    let p = path.trim();
    if p.is_empty() || p == "root.**" {
        (None, None)
    } else {
        (Some(p.to_string()), Some("database".to_string()))
    }
}

fn truthy(v: Option<&J>) -> bool {
    match v {
        Some(J::Bool(b)) => *b,
        Some(J::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// `LIST PRIVILEGES OF …` rows: role (empty when direct), path, privilege,
/// grant option. `own_role`: the role being listed, whose rows aren't "via".
fn grants_of(rows: &[Vec<J>], own_role: Option<&str>) -> Vec<Grant> {
    rows.iter()
        .filter_map(|r| {
            let privilege = r.get(2).map(text).filter(|p| !p.is_empty())?;
            let role = r.first().map(text).unwrap_or_default();
            let (object, object_kind) = scope(&r.get(1).map(text).unwrap_or_default());
            let via = (!role.is_empty() && Some(role.as_str()) != own_role).then(|| role_name(&role));
            Some(Grant { privilege, object, object_kind, grantable: truthy(r.get(3)), denied: false, via })
        })
        .collect()
}

pub async fn grants(s: &IotDbSession, principal: &str) -> Result<Vec<Grant>> {
    let (role, name) = grantee(principal);
    let what = if role { "ROLE" } else { "USER" };
    let t = s.query(&format!("LIST PRIVILEGES OF {what} {}", ident(name)), 100_000).await?;
    let mut g = grants_of(&t.rows, role.then_some(name));
    if !role && name == "root" && g.is_empty() {
        g.push(Grant { privilege: "ALL".into(), ..Default::default() });
    }
    Ok(g)
}

// -- scripts -------------------------------------------------------------------

/// A path pattern as IoTDB takes it: each node quoted unless it's plain or
/// a wildcard.
fn path_pattern(p: &str) -> Result<String> {
    let nodes = split_path(p.trim());
    if nodes.first().map(String::as_str) != Some("root") || nodes.iter().any(|n| n.is_empty()) {
        return Err(Error::Query(format!("«{p}» no es una ruta de IoTDB (root.…)")));
    }
    Ok(nodes.iter().map(|n| if n == "*" || n == "**" || n == "root" { n.clone() } else { node(n) }).collect::<Vec<_>>().join("."))
}

/// What a privilege applies to: every path, or a database's subtree (a
/// path pattern as a grant lists it goes as is).
fn on(object: &Option<ObjectRef>) -> Result<String> {
    match object {
        None => Ok("root.**".into()),
        Some(o) if o.name.contains('*') => path_pattern(&o.name),
        Some(o) => path_pattern(&format!("{}.**", database_path(&o.name))),
    }
}

fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let up = x.trim().to_uppercase();
        if up.is_empty() || !up.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
            return Err(Error::Query(format!("«{x}» no es un permiso de IoTDB")));
        }
        out.push(up);
    }
    Ok(out.join(", "))
}

fn to_whom(name: &str) -> String {
    match grantee(name) {
        (true, r) => format!("ROLE {}", ident(r)),
        (false, u) => format!("USER {}", ident(u)),
    }
}

/// A role's name in IoTDB, from DBine's (`role:x`) or as typed.
fn bare_role(name: &str) -> &str {
    grantee(name).1
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} {};", ident(name), lit(pw))
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", ident(bare_role(name))),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", ident(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", ident(bare_role(name))),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} SET PASSWORD {};", ident(name), lit(password)),
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported("IoTDB no permite deshabilitar un usuario: cambiale la contraseña o borralo".into()))
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => format!(
            "GRANT {} ON {} TO {}{};",
            privileges(p)?,
            on(object)?,
            to_whom(to),
            if *grantable { " WITH GRANT OPTION" } else { "" }
        ),
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {} ON {} FROM {};", privileges(p)?, on(object)?, to_whom(from)),
        SecurityAction::AddMember { role, member } => {
            if grantee(member).0 {
                return Err(Error::Unsupported("en IoTDB un rol no puede ser miembro de otro rol".into()));
            }
            format!("GRANT ROLE {} TO {};", ident(bare_role(role)), ident(member))
        }
        SecurityAction::RemoveMember { role, member } => format!("REVOKE ROLE {} FROM {};", ident(bare_role(role)), ident(grantee(member).1)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db(name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: "database".into(), schema: None, name: name.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w".into()) }), "CREATE USER ana 'p''w';");
        assert_eq!(s(SecurityAction::CreateUser { name: "ana-b`".into(), password: Some("x".into()) }), "CREATE USER `ana-b``` 'x';");
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "lect".into() }), "CREATE ROLE lect;");
        assert_eq!(s(SecurityAction::Drop { name: "role:lect".into(), kind: PrincipalKind::Role }), "DROP ROLE lect;");
        assert_eq!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), "DROP USER ana;");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "n'1".into() }), "ALTER USER ana SET PASSWORD 'n''1';");
        assert!(matches!(script(&SecurityAction::SetLogin { name: "ana".into(), enabled: false }), Err(Error::Unsupported(_))));
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["read_data".into(), "WRITE_DATA".into()], object: db("root.sg"), to: "ana".into(), grantable: true }),
            "GRANT READ_DATA, WRITE_DATA ON root.sg.** TO USER ana WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["READ_DATA".into()], object: db("s g"), to: "role:lect".into(), grantable: false }),
            "GRANT READ_DATA ON root.`s g`.** TO ROLE lect;"
        );
        assert_eq!(s(SecurityAction::Grant { privileges: vec!["MANAGE_USER".into()], object: None, to: "ana".into(), grantable: false }), "GRANT MANAGE_USER ON root.** TO USER ana;");
        // A path pattern as a grant lists it.
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["READ_DATA".into()], object: db("root.`s g`.d1.**"), from: "role:lect".into() }),
            "REVOKE READ_DATA ON root.`s g`.d1.** FROM ROLE lect;"
        );
        assert_eq!(s(SecurityAction::AddMember { role: "role:lect".into(), member: "ana".into() }), "GRANT ROLE lect TO ana;");
        assert_eq!(s(SecurityAction::RemoveMember { role: "lect".into(), member: "ana".into() }), "REVOKE ROLE lect FROM ana;");
        assert!(script(&SecurityAction::AddMember { role: "role:a".into(), member: "role:b".into() }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["READ_DATA ON root.** TO USER x; --".into()], object: None, to: "a".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["READ_DATA".into()], object: db("other.**"), to: "a".into(), grantable: false }).is_err());
    }

    #[test]
    fn reads_privileges() {
        let rows = vec![
            vec![json!(""), json!(""), json!("MANAGE_DATABASE"), json!(true)],
            vec![json!(""), json!("root.sg.**"), json!("WRITE_SCHEMA"), json!(false)],
            vec![json!("lect"), json!("root.**"), json!("READ_DATA"), json!(false)],
        ];
        let g = grants_of(&rows, None);
        assert_eq!(g.len(), 3);
        assert_eq!((g[0].object.clone(), g[0].grantable, g[0].via.clone()), (None, true, None));
        assert_eq!((g[1].object.as_deref(), g[1].object_kind.as_deref()), (Some("root.sg.**"), Some("database")));
        assert_eq!(g[2].via.as_deref(), Some("role:lect"));
        let own = grants_of(&rows[2..], Some("lect"));
        assert_eq!(own[0].via, None);
    }
}

//! H2 in PostgreSQL server mode (`-pg`): users and roles live in its own
//! `INFORMATION_SCHEMA` (`USERS`, `ROLES`, `RIGHTS`), not in `pg_roles`.
//!
//! - Users are either admins (`ADMIN TRUE`, every right) or not; there's no
//!   way to disable one, and `CREATE USER` needs a password.
//! - `RIGHTS` has one row per grantee and object, with the privileges in one
//!   comma-separated cell; a row with `GRANTEDROLE` is a role membership.
//! - Database-wide there are two rights: `ALTER ANY SCHEMA` (a `GRANT`) and
//!   `ADMIN` (`ALTER USER … ADMIN TRUE`), both offered on "" (the whole
//!   database).
//! - No `WITH GRANT OPTION`.

use super::{privileges, q, yes};
use crate::catalog::{cell, lit};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::HashMap;

const ADMIN: &str = "ADMIN";
const ALTER_ANY_SCHEMA: &str = "ALTER ANY SCHEMA";

pub(super) fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALL PRIVILEGES", ALTER_ANY_SCHEMA, ADMIN],
        object_kinds: vec!["", "schema", "table", "view"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

/// (member, role) pairs.
pub(super) const MEMBERSHIPS: &str = "SELECT grantee AS member, grantedrole AS role
       FROM INFORMATION_SCHEMA.RIGHTS WHERE grantedrole IS NOT NULL AND grantedrole <> ''";

pub(super) async fn principals(s: &PgSession) -> Result<Vec<Principal>> {
    let users = s.text("SELECT user_name AS name, is_admin AS admin, remarks FROM INFORMATION_SCHEMA.USERS ORDER BY user_name").await?;
    let mut out: Vec<Principal> = users.iter().map(|r| user(cell(r, "name").unwrap_or_default(), yes(r, "admin"), cell(r, "remarks"))).collect();
    let roles = s.text("SELECT role_name AS name, remarks FROM INFORMATION_SCHEMA.ROLES ORDER BY role_name").await?;
    out.extend(roles.iter().map(|r| role(cell(r, "name").unwrap_or_default(), cell(r, "remarks"))));
    Ok(out)
}

fn remarks(details: &mut Vec<(String, String)>, remarks: Option<String>) {
    if let Some(r) = remarks.filter(|r| !r.is_empty()) {
        details.push(("Comentario".into(), r));
    }
}

fn user(name: String, admin: bool, comment: Option<String>) -> Principal {
    let mut details = vec![("Tipo".to_string(), if admin { "Usuario administrador" } else { "Usuario" }.to_string())];
    remarks(&mut details, comment);
    Principal {
        kind: PrincipalKind::User,
        can_login: Some(true),
        superuser: Some(admin),
        // H2 can't disable a user.
        disabled: None,
        member_of: Vec::new(),
        details,
        system: false,
        name,
    }
}

fn role(name: String, comment: Option<String>) -> Principal {
    let mut details = vec![("Tipo".to_string(), "Rol".to_string())];
    remarks(&mut details, comment);
    Principal {
        kind: PrincipalKind::Role,
        can_login: Some(false),
        // PUBLIC: every user is in it.
        system: name.eq_ignore_ascii_case("public"),
        name,
        details,
        ..Default::default()
    }
}

pub(super) async fn grants(s: &PgSession, principal: &str, via: &HashMap<String, Option<String>>) -> Result<Vec<(String, Grant)>> {
    let names = via.keys().map(|n| lit(Variant::H2, n)).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT r.grantee AS grantee, r.rights AS rights, r.table_schema AS sch, r.table_name AS tbl, t.table_type AS ttype
           FROM INFORMATION_SCHEMA.RIGHTS r
           LEFT JOIN INFORMATION_SCHEMA.TABLES t ON t.table_schema = r.table_schema AND t.table_name = r.table_name
          WHERE (r.grantedrole IS NULL OR r.grantedrole = '') AND r.grantee IN ({names})"
    );
    let mut out: Vec<(String, Grant)> = Vec::new();
    for r in s.text(&sql).await? {
        let Some(grantee) = cell(&r, "grantee") else { continue };
        let rights = cell(&r, "rights").unwrap_or_default();
        out.extend(
            rights_row(&rights, cell(&r, "sch"), cell(&r, "tbl"), cell(&r, "ttype").as_deref())
                .into_iter()
                .map(|g| (grantee.clone(), g)),
        );
    }
    // An admin's ADMIN right isn't in RIGHTS.
    let admin = s
        .text(&format!("SELECT is_admin AS admin FROM INFORMATION_SCHEMA.USERS WHERE user_name = {}", lit(Variant::H2, principal)))
        .await?;
    if admin.first().is_some_and(|r| yes(r, "admin")) {
        out.push((principal.to_string(), Grant { privilege: ADMIN.into(), ..Default::default() }));
    }
    Ok(out)
}

/// The grants of one `RIGHTS` row: `rights` is `SELECT, UPDATE`; an empty
/// table is the whole schema, and with no schema either, the database.
fn rights_row(rights: &str, schema: Option<String>, table: Option<String>, table_type: Option<&str>) -> Vec<Grant> {
    let schema = schema.filter(|s| !s.is_empty());
    let table = table.filter(|t| !t.is_empty());
    let (object, kind) = match (schema, table) {
        (Some(sc), Some(t)) => {
            let kind = if table_type == Some("VIEW") { "view" } else { "table" };
            (Some(format!("{sc}.{t}")), Some(kind.to_string()))
        }
        (Some(sc), None) => (Some(sc), Some("schema".to_string())),
        (None, Some(t)) => (Some(t), Some("table".to_string())),
        (None, None) => (None, None),
    };
    rights
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| Grant { privilege: p.to_string(), object: object.clone(), object_kind: kind.clone(), ..Default::default() })
        .collect()
}

fn on(o: &ObjectRef) -> String {
    match o.kind.as_str() {
        "schema" => format!("SCHEMA {}", q(&o.name)),
        _ => qualified_name(Quote::Double, o.schema(), &o.name),
    }
}

fn database_wide(p: &str) -> bool {
    p == ADMIN || p == ALTER_ANY_SCHEMA
}

/// `GRANT`/`REVOKE` of `p` on the database: one statement per right.
fn on_database(p: &[String], name: &str, grant: bool) -> Result<String> {
    let mut out = Vec::new();
    for x in privileges(p)?.split(", ") {
        out.push(match x {
            ADMIN => format!("ALTER USER {} ADMIN {};", q(name), if grant { "TRUE" } else { "FALSE" }),
            ALTER_ANY_SCHEMA if grant => format!("GRANT ALTER ANY SCHEMA TO {};", q(name)),
            ALTER_ANY_SCHEMA => format!("REVOKE ALTER ANY SCHEMA FROM {};", q(name)),
            other => return Err(Error::Query(format!("en H2 {other} se otorga sobre un esquema, una tabla o una vista"))),
        });
    }
    Ok(out.join("\n"))
}

fn on_object(p: &[String]) -> Result<String> {
    let list = privileges(p)?;
    if let Some(x) = list.split(", ").find(|x| database_wide(x)) {
        return Err(Error::Query(format!("en H2 {x} es sobre toda la base, no sobre un objeto")));
    }
    Ok(list)
}

pub(super) fn script(a: &SecurityAction) -> Result<String> {
    let v = Variant::H2;
    Ok(match a {
        SecurityAction::CreateUser { name, password } => match password.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("CREATE USER {} PASSWORD {};", q(name), lit(v, p)),
            None => return Err(Error::Query("H2 pide una contraseña para crear un usuario".into())),
        },
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(name)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} SET PASSWORD {};", q(name), lit(v, password)),
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported("H2 no deshabilita usuarios: se les puede cambiar la contraseña o borrarlos".into()))
        }
        SecurityAction::Grant { grantable: true, .. } => {
            return Err(Error::Query("H2 no permite que un usuario otorgue a otros sus permisos (no hay WITH GRANT OPTION)".into()))
        }
        SecurityAction::Grant { privileges: p, object: None, to, .. } => on_database(p, to, true)?,
        SecurityAction::Grant { privileges: p, object: Some(o), to, .. } => format!("GRANT {} ON {} TO {};", on_object(p)?, on(o), q(to)),
        SecurityAction::Revoke { privileges: p, object: None, from } => on_database(p, from, false)?,
        SecurityAction::Revoke { privileges: p, object: Some(o), from } => format!("REVOKE {} ON {} FROM {};", on_object(p)?, on(o), q(from)),
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
    fn scripts_quote_and_escape() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana\"x".into(), password: Some("p'w\\".into()) }),
            "CREATE USER \"ana\"\"x\" PASSWORD 'p''w\\';"
        );
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: Some(String::new()) }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "lec".into() }), "CREATE ROLE \"lec\";");
        assert_eq!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), "DROP USER \"ana\";");
        assert_eq!(s(SecurityAction::Drop { name: "lec".into(), kind: PrincipalKind::Role }), "DROP ROLE \"lec\";");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "x'y".into() }), "ALTER USER \"ana\" SET PASSWORD 'x''y';");
        assert!(script(&SecurityAction::SetLogin { name: "ana".into(), enabled: false }).is_err());
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), " update ".into()], object: table(), to: "ana".into(), grantable: false }),
            "GRANT SELECT, UPDATE ON \"pub\"\"lic\".\"fac\"\"turas\" TO \"ana\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke {
                privileges: vec!["insert".into()],
                object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "ven\"tas".into() }),
                from: "r1".into()
            }),
            "REVOKE INSERT ON SCHEMA \"ven\"\"tas\" FROM \"r1\";"
        );
        assert_eq!(s(SecurityAction::AddMember { role: "lec".into(), member: "ana".into() }), "GRANT \"lec\" TO \"ana\";");
        assert_eq!(s(SecurityAction::RemoveMember { role: "lec".into(), member: "ana".into() }), "REVOKE \"lec\" FROM \"ana\";");
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: table(), to: "a".into(), grantable: true }).is_err());
    }

    #[test]
    fn database_rights() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["admin".into(), "alter any schema".into()], object: None, to: "a\"b".into(), grantable: false }),
            "ALTER USER \"a\"\"b\" ADMIN TRUE;\nGRANT ALTER ANY SCHEMA TO \"a\"\"b\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["ADMIN".into(), "ALTER  ANY SCHEMA".into()], object: None, from: "a".into() }),
            "ALTER USER \"a\" ADMIN FALSE;\nREVOKE ALTER ANY SCHEMA FROM \"a\";"
        );
        // Table privileges need an object, and database rights can't have one.
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: None, to: "a".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["ADMIN".into()], object: table(), to: "a".into(), grantable: false }).is_err());
    }

    #[test]
    fn privilege_names_cannot_inject() {
        for bad in ["SELECT; DROP TABLE x", "SELECT --", "SEL'ECT", "", "ADMIN TRUE; DROP USER sa"] {
            let a = SecurityAction::Grant { privileges: vec![bad.into()], object: table(), to: "a".into(), grantable: false };
            assert!(script(&a).is_err(), "{bad}");
            let a = SecurityAction::Grant { privileges: vec![bad.into()], object: None, to: "a".into(), grantable: false };
            assert!(script(&a).is_err(), "{bad}");
        }
    }

    #[test]
    fn rights_rows() {
        let s = |x: &str| Some(x.to_string());
        let g = rights_row("SELECT, UPDATE", s("public"), s("facturas"), Some("BASE TABLE"));
        assert_eq!(g.len(), 2);
        assert_eq!((g[1].privilege.as_str(), g[1].object.as_deref(), g[1].object_kind.as_deref()), ("UPDATE", Some("public.facturas"), Some("table")));
        let g = rights_row("SELECT", s("public"), s("v"), Some("VIEW"));
        assert_eq!(g[0].object_kind.as_deref(), Some("view"));
        let g = rights_row("INSERT", s("public"), s(""), None);
        assert_eq!((g[0].object.as_deref(), g[0].object_kind.as_deref()), (Some("public"), Some("schema")));
        let g = rights_row("ALTER ANY SCHEMA", s(""), None, None);
        assert_eq!((g[0].privilege.as_str(), g[0].object.as_deref(), g[0].object_kind.as_deref()), ("ALTER ANY SCHEMA", None, None));
        assert!(rights_row("", s("public"), s("t"), None).is_empty());
    }

    #[test]
    fn principals_from_the_catalog() {
        let u = user("sa".into(), true, Some(String::new()));
        assert_eq!((u.kind, u.superuser, u.disabled, u.system), (PrincipalKind::User, Some(true), None, false));
        assert_eq!(u.details, vec![("Tipo".to_string(), "Usuario administrador".to_string())]);
        let r = role("public".into(), Some("todos".into()));
        assert!(r.system && r.kind == PrincipalKind::Role);
        assert!(r.details.contains(&("Comentario".to_string(), "todos".to_string())));
        assert!(!role("lectores".into(), None).system);
    }
}

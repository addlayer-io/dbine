//! Apache Doris (and VeloDB, managed Doris): users `'user'@'host'`, roles,
//! and privileges named `SELECT_PRIV`, `LOAD_PRIV`… on a level
//! (`*.*.*`, `ctl.*.*`, `ctl.db.*`, `ctl.db.tbl`, `RESOURCE 'r'`,
//! `WORKLOAD GROUP 'g'`…).
//!
//! Users are named `user@host`, roles `role:<name>`. Doris has no WITH
//! GRANT OPTION (GRANT_PRIV instead), no roles inside roles and can't lock
//! an account by hand (ACCOUNT_UNLOCK only undoes a lock from failed
//! logins). Grants are read from `SHOW GRANTS FOR` / `SHOW ALL GRANTS`
//! (one column per level: `internal.db.t: Select_priv,Load_priv; …`),
//! which merges in the user's roles; a grant is direct when none of its
//! roles (`SHOW ROLES`) has it.

use super::common::{bare_role, q, role, unsupported, who, Who};
use super::starrocks::identity;
use super::{account, new_user, split_top, unquote};
use crate::session::{lit, named, MySqlSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use mysql_async::Row;
use std::collections::HashSet;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT_PRIV", "LOAD_PRIV", "ALTER_PRIV", "CREATE_PRIV", "DROP_PRIV", "SHOW_VIEW_PRIV", "USAGE_PRIV", "GRANT_PRIV", "ADMIN_PRIV",
            "NODE_PRIV",
        ],
        // "" = everything (`*.*.*`), "schema" = one database (`db.*`).
        // Table grants are read and revoked, but not offered: the explorer
        // hands over a table without its database and Doris reads a lone
        // name as a database.
        object_kinds: vec!["", "schema"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

/// Built-in accounts and roles.
const SYSTEM_USERS: [&str; 2] = ["root", "admin"];
const SYSTEM_ROLES: [&str; 2] = ["operator", "admin"];

// -- reading -----------------------------------------------------------------

type Triple = (String, Option<String>, Option<String>);

/// Privilege columns and the object kind their keys name.
const COLUMNS: [(&str, &str); 9] = [
    ("GlobalPrivs", ""),
    ("CatalogPrivs", "catalog"),
    ("DatabasePrivs", "schema"),
    ("TablePrivs", "table"),
    ("ColPrivs", "table"),
    ("ResourcePrivs", "resource"),
    ("WorkloadGroupPrivs", "workload_group"),
    ("ComputeGroupPrivs", "compute_group"),
    ("StorageVaultPrivs", "storage_vault"),
];

/// A key of a privilege column as DBine names the object: the internal
/// catalog goes unsaid (`internal.db.t` → `db.t`), a role's `db.*` is the
/// database.
fn object_name(key: &str, kind: &str) -> String {
    let key = key.trim();
    let key = if kind == "schema" { key.strip_suffix(".*").unwrap_or(key) } else { key };
    let key = if kind == "schema" || kind == "table" { key.strip_prefix("internal.").unwrap_or(key) } else { key };
    if kind == "resource" && key == "%" {
        return "*".into();
    }
    key.to_string()
}

/// One privilege: `Select_priv` → `SELECT_PRIV`; older releases add
/// `  (false)`; column privileges read `Select_priv[id, total]`.
fn privilege(p: &str) -> Option<String> {
    let p = p.trim();
    let p = p.split("  (").next().unwrap_or(p).trim();
    if p.is_empty() {
        return None;
    }
    Some(match p.split_once('[') {
        Some((n, cols)) => format!("{}({})", n.trim().to_uppercase(), cols.trim_end_matches(']').trim()),
        None => p.to_uppercase(),
    })
}

/// A privilege column: `internal.db: Select_priv,Load_priv; internal.x: …`
/// (the global one is only the list).
pub(super) fn parse_column(text: &str, kind: &str) -> Vec<Triple> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("NULL") {
        return Vec::new();
    }
    let mut out = Vec::new();
    if kind.is_empty() {
        out.extend(text.split(',').filter_map(privilege).map(|p| (p, None, None)));
        return out;
    }
    for entry in text.split("; ") {
        let Some((key, privs)) = entry.split_once(": ") else { continue };
        let object = object_name(key, kind);
        // Everyone reads information_schema and mysql, and uses the
        // normal workload group: not grants of this principal.
        if (kind == "schema" && (object == "information_schema" || object == "mysql")) || (kind == "workload_group" && object == "normal") {
            continue;
        }
        // Column privileges: `Select_priv[id]` split by commas outside brackets.
        for p in split_privs(privs).into_iter().filter_map(|p| privilege(&p)) {
            out.push((p, Some(object.clone()), Some(kind.to_string())));
        }
    }
    out
}

fn split_privs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut depth) = (String::new(), 0);
    for c in s.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out
}

fn row_grants(r: &Row) -> Vec<Triple> {
    COLUMNS.iter().flat_map(|(col, kind)| parse_column(&named(r, &[col]).unwrap_or_default(), kind)).collect()
}

fn list(s: Option<String>) -> Vec<String> {
    s.unwrap_or_default().split(',').map(str::trim).filter(|r| !r.is_empty()).map(unquote).collect()
}

pub async fn principals(s: &mut MySqlSession) -> Result<Vec<Principal>> {
    let rows = match s.rows("SHOW ALL GRANTS").await {
        Ok(r) => r,
        // Without GRANT_PRIV: only one's own.
        Err(_) => s.rows("SHOW GRANTS").await?,
    };
    let mut out = Vec::new();
    for r in &rows {
        let name = identity(&named(r, &["UserIdentity"]).unwrap_or_default());
        let user = name.rsplit_once('@').map_or(name.as_str(), |(u, _)| u).to_string();
        let mut details = vec![("Host".into(), name.rsplit_once('@').map_or("%", |(_, h)| h).to_string())];
        if let Some(c) = named(r, &["Comment"]).filter(|c| !c.is_empty() && c != "NULL") {
            details.push(("Comentario".into(), c));
        }
        if named(r, &["Password"]).is_some_and(|p| p.eq_ignore_ascii_case("No")) {
            details.push(("Contraseña".into(), "sin contraseña".into()));
        }
        let global = named(r, &["GlobalPrivs"]).unwrap_or_default();
        out.push(Principal {
            member_of: list(named(r, &["Roles"])).iter().map(|x| role(x)).collect(),
            superuser: Some(global.contains("Admin_priv") || global.contains("ADMIN_PRIV")),
            can_login: Some(true),
            kind: PrincipalKind::User,
            system: SYSTEM_USERS.contains(&user.as_str()),
            details,
            name,
            ..Default::default()
        });
    }
    for r in s.optional_rows("SHOW ROLES").await {
        let name = named(&r, &["Name"]).unwrap_or_default();
        let global = named(&r, &["GlobalPrivs"]).unwrap_or_default();
        let mut details = Vec::new();
        if let Some(c) = named(&r, &["Comment"]).filter(|c| !c.is_empty() && c != "NULL") {
            details.push(("Comentario".into(), c));
        }
        out.push(Principal {
            name: role(&name),
            kind: PrincipalKind::Role,
            superuser: Some(global.contains("Admin_priv")),
            system: SYSTEM_ROLES.contains(&name.as_str()),
            details,
            ..Default::default()
        });
    }
    Ok(out)
}

pub async fn grants(s: &mut MySqlSession, principal: &str) -> Result<Vec<Grant>> {
    let roles = s.optional_rows("SHOW ROLES").await;
    let role_row = |n: &str| roles.iter().find(|r| named(r, &["Name"]).as_deref() == Some(n));
    let grant = |(privilege, object, object_kind): Triple, via: Option<String>| Grant { privilege, object, object_kind, via, ..Default::default() };
    match who(principal) {
        Who::Role(r) | Who::Group(r) => {
            let row = role_row(r).ok_or_else(|| Error::Query(format!("no existe el rol «{r}»")))?;
            Ok(row_grants(row).into_iter().map(|t| grant(t, None)).collect())
        }
        Who::User(u) => {
            let rows = s.rows(&format!("SHOW GRANTS FOR {}", account(u))).await?;
            let Some(row) = rows.first() else { return Ok(Vec::new()) };
            let mut through = Vec::new();
            let mut seen = HashSet::new();
            for r in list(named(row, &["Roles"])) {
                for t in role_row(&r).map(row_grants).unwrap_or_default() {
                    if seen.insert(t.clone()) {
                        through.push(grant(t, Some(role(&r))));
                    }
                }
            }
            let mut out: Vec<Grant> = row_grants(row).into_iter().filter(|t| !seen.contains(t)).map(|t| grant(t, None)).collect();
            out.append(&mut through);
            Ok(out)
        }
    }
}

// -- scripts -----------------------------------------------------------------

/// `SELECT_PRIV` or `SELECT_PRIV(id, total)` (columns quoted); anything
/// else is rejected.
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let bad = |x: &str| Error::Query(format!("«{x}» no es un permiso de Doris"));
    let mut out = Vec::new();
    for x in p {
        let (name, cols) = match x.split_once('(') {
            Some((n, c)) => (n.trim(), Some(c.trim().strip_suffix(')').ok_or_else(|| bad(x))?)),
            None => (x.trim(), None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
            return Err(bad(x));
        }
        let name = name.to_uppercase();
        match cols {
            Some(c) => {
                let cols: Vec<String> = c.split(',').map(|c| unquote(c.trim())).filter(|c| !c.is_empty()).map(|c| q(&c)).collect();
                if cols.is_empty() {
                    return Err(bad(x));
                }
                out.push(format!("{name}({})", cols.join(", ")));
            }
            None => out.push(name),
        }
    }
    Ok(out.join(", "))
}

/// The privilege level.
fn level(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else { return Ok("*.*.*".into()) };
    // `ctl.db` / `db.t` / `ctl.db.t`: Doris names have no dots.
    let parts: Vec<&str> = o.schema().into_iter().chain(o.name.split('.')).collect();
    let dotted = |ps: &[&str]| ps.iter().map(|p| if *p == "*" { "*".to_string() } else { q(p) }).collect::<Vec<_>>().join(".");
    Ok(match o.kind.as_str() {
        "catalog" => format!("{}.*.*", q(&o.name)),
        "schema" | "database" => format!("{}.*", dotted(&split_top(&o.name, ".").into_iter().collect::<Vec<_>>())),
        "table" | "view" | "materialized_view" if parts.len() >= 2 => dotted(&parts),
        "table" | "view" | "materialized_view" => return Err(Error::Query(format!("indicá la base de «{}»: Doris lee un nombre solo como una base", o.name))),
        "resource" => format!("RESOURCE {}", lit(&o.name)),
        "workload_group" => format!("WORKLOAD GROUP {}", lit(&o.name)),
        "compute_group" => format!("COMPUTE GROUP {}", lit(&o.name)),
        "storage_vault" => format!("STORAGE VAULT {}", lit(&o.name)),
        k => return Err(Error::Query(format!("Doris no otorga permisos sobre «{k}»"))),
    })
}

fn grantee(name: &str) -> Result<String> {
    Ok(match who(name) {
        Who::Role(r) => format!("ROLE {}", lit(r)),
        Who::User(u) => account(u),
        Who::Group(_) => return Err(unsupported("Doris no tiene grupos")),
    })
}

/// The member of a role: only users.
fn member(name: &str) -> Result<String> {
    match who(name) {
        Who::User(u) => Ok(account(u)),
        _ => Err(unsupported("Doris no permite roles dentro de roles: solo los usuarios son miembros")),
    }
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
        SecurityAction::SetPassword { name, password } => format!("SET PASSWORD FOR {} = PASSWORD({});", account(name), lit(password)),
        SecurityAction::SetLogin { name, enabled: true } => format!("ALTER USER {} ACCOUNT_UNLOCK;", account(name)),
        SecurityAction::SetLogin { enabled: false, .. } => {
            return Err(unsupported("Doris no permite deshabilitar el ingreso de un usuario: cambiale la contraseña o borralo"))
        }
        SecurityAction::Grant { grantable: true, .. } => {
            return Err(unsupported("Doris no tiene WITH GRANT OPTION: para que pueda otorgar, dale GRANT_PRIV"))
        }
        SecurityAction::Grant { privileges: p, object, to, .. } => format!("GRANT {} ON {} TO {};", privileges(p)?, level(object)?, grantee(to)?),
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {} ON {} FROM {};", privileges(p)?, level(object)?, grantee(from)?),
        SecurityAction::AddMember { role, member: m } => format!("GRANT {} TO {};", lit(bare_role(role)), member(m)?),
        SecurityAction::RemoveMember { role, member: m } => format!("REVOKE {} FROM {};", lit(bare_role(role)), member(m)?),
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
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w".into()) }), "CREATE USER 'ana'@'%' IDENTIFIED BY 'p''w';");
        assert_eq!(s(SecurityAction::CreateRole { name: "role:lect".into() }), "CREATE ROLE `lect`;");
        assert_eq!(s(SecurityAction::Drop { name: "role:lect".into(), kind: PrincipalKind::Role }), "DROP ROLE `lect`;");
        assert_eq!(s(SecurityAction::Drop { name: "ana@%".into(), kind: PrincipalKind::User }), "DROP USER 'ana'@'%';");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana@%".into(), password: "n'".into() }), "SET PASSWORD FOR 'ana'@'%' = PASSWORD('n''');");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana@%".into(), enabled: true }), "ALTER USER 'ana'@'%' ACCOUNT_UNLOCK;");
        assert!(matches!(script(&SecurityAction::SetLogin { name: "ana@%".into(), enabled: false }), Err(Error::Unsupported(_))));
        let grant = |p: &[&str], object, to: &str| SecurityAction::Grant { privileges: p.iter().map(|x| x.to_string()).collect(), object, to: to.into(), grantable: false };
        assert_eq!(s(grant(&["select_priv", "LOAD_PRIV"], obj("table", Some("ventas"), "fac`t"), "ana@%")), "GRANT SELECT_PRIV, LOAD_PRIV ON `ventas`.`fac``t` TO 'ana'@'%';");
        assert_eq!(s(grant(&["SELECT_PRIV(id, `total`)"], obj("table", Some("v"), "f"), "role:r")), "GRANT SELECT_PRIV(`id`, `total`) ON `v`.`f` TO ROLE 'r';");
        assert_eq!(s(grant(&["LOAD_PRIV"], obj("schema", None, "ventas"), "role:r")), "GRANT LOAD_PRIV ON `ventas`.* TO ROLE 'r';");
        assert_eq!(s(grant(&["LOAD_PRIV"], obj("schema", None, "hive.ventas"), "role:r")), "GRANT LOAD_PRIV ON `hive`.`ventas`.* TO ROLE 'r';");
        assert_eq!(s(grant(&["NODE_PRIV"], None, "role:r")), "GRANT NODE_PRIV ON *.*.* TO ROLE 'r';");
        assert_eq!(s(grant(&["CREATE_PRIV"], obj("catalog", None, "internal"), "ana@%")), "GRANT CREATE_PRIV ON `internal`.*.* TO 'ana'@'%';");
        assert_eq!(s(grant(&["USAGE_PRIV"], obj("resource", None, "*"), "ana@%")), "GRANT USAGE_PRIV ON RESOURCE '*' TO 'ana'@'%';");
        assert_eq!(s(grant(&["USAGE_PRIV"], obj("workload_group", None, "g'1"), "ana@%")), "GRANT USAGE_PRIV ON WORKLOAD GROUP 'g''1' TO 'ana'@'%';");
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["ALTER_PRIV".into()], object: obj("table", Some("hive"), "db.t"), from: "role:r".into() }),
            "REVOKE ALTER_PRIV ON `hive`.`db`.`t` FROM ROLE 'r';"
        );
        assert!(script(&grant(&["SELECT_PRIV"], obj("table", None, "f"), "ana@%")).is_err());
        assert!(script(&grant(&["SELECT_PRIV; DROP"], None, "ana@%")).is_err());
        assert!(script(&grant(&["SELECT_PRIV(a"], None, "ana@%")).is_err());
        assert!(matches!(
            script(&SecurityAction::Grant { privileges: vec!["SELECT_PRIV".into()], object: None, to: "ana@%".into(), grantable: true }),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(s(SecurityAction::AddMember { role: "role:lect".into(), member: "ana@%".into() }), "GRANT 'lect' TO 'ana'@'%';");
        assert_eq!(s(SecurityAction::RemoveMember { role: "role:lect".into(), member: "ana@%".into() }), "REVOKE 'lect' FROM 'ana'@'%';");
        assert!(matches!(script(&SecurityAction::AddMember { role: "role:lect".into(), member: "role:sup".into() }), Err(Error::Unsupported(_))));
    }

    #[test]
    fn parses_privilege_columns() {
        let t = |p: &str, o: Option<&str>, k: Option<&str>| (p.to_string(), o.map(String::from), k.map(String::from));
        assert_eq!(parse_column("Node_priv,Admin_priv", ""), vec![t("NODE_PRIV", None, None), t("ADMIN_PRIV", None, None)]);
        assert_eq!(parse_column("NULL", "schema"), vec![]);
        assert_eq!(
            parse_column("internal.dbine_secx: Select_priv,Load_priv; internal.information_schema: Select_priv; internal.mysql: Select_priv", "schema"),
            vec![t("SELECT_PRIV", Some("dbine_secx"), Some("schema")), t("LOAD_PRIV", Some("dbine_secx"), Some("schema"))]
        );
        // A role's database key ends in `.*`.
        assert_eq!(parse_column("internal.dbine_secx.*: Drop_priv", "schema"), vec![t("DROP_PRIV", Some("dbine_secx"), Some("schema"))]);
        assert_eq!(parse_column("hive.ventas: Select_priv", "schema"), vec![t("SELECT_PRIV", Some("hive.ventas"), Some("schema"))]);
        assert_eq!(
            parse_column("internal.dbine_secx.f: Select_priv,Alter_priv; internal.dbine_secx.v: Show_view_priv", "table"),
            vec![t("SELECT_PRIV", Some("dbine_secx.f"), Some("table")), t("ALTER_PRIV", Some("dbine_secx.f"), Some("table")), t("SHOW_VIEW_PRIV", Some("dbine_secx.v"), Some("table"))]
        );
        assert_eq!(parse_column("internal.d.f: Select_priv[id, total],Load_priv[id]", "table"), vec![t("SELECT_PRIV(id, total)", Some("d.f"), Some("table")), t("LOAD_PRIV(id)", Some("d.f"), Some("table"))]);
        assert_eq!(parse_column("internal: Create_priv", "catalog"), vec![t("CREATE_PRIV", Some("internal"), Some("catalog"))]);
        assert_eq!(parse_column("%: Usage_priv", "resource"), vec![t("USAGE_PRIV", Some("*"), Some("resource"))]);
        assert_eq!(parse_column("normal: Usage_priv; etl: Usage_priv", "workload_group"), vec![t("USAGE_PRIV", Some("etl"), Some("workload_group"))]);
        // Doris 1.x / 2.0: `Select_priv  (false)`.
        assert_eq!(parse_column("internal.d: Select_priv  (false)", "schema"), vec![t("SELECT_PRIV", Some("d"), Some("schema"))]);
    }
}

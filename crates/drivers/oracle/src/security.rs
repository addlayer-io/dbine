//! Users, roles and permissions (docs/users-and-permissions.md). Oracle's
//! users are also its schemas; privileges are system privileges (`CREATE
//! TABLE`, `SELECT ANY TABLE`…) or object privileges on one object.
//!
//! The DBA_ views show every user, role and grant (they need SELECT ANY
//! DICTIONARY / SELECT_CATALOG_ROLE, which DBAs have). Without them it falls
//! back to ALL_USERS and the USER_ / ROLE_ views: every user, but only the
//! roles and grants of the session's own user and of its enabled roles.

use crate::monitor::txt;
use crate::{cell, err, quote as q, schema_name};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use oracledb::{Connection, ToDbValue};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            // On objects.
            "SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "REFERENCES", "ALTER", "INDEX", "READ", "DEBUG",
            // System-wide (granted without an object).
            "CREATE SESSION", "CREATE TABLE", "CREATE VIEW", "CREATE SEQUENCE", "CREATE PROCEDURE", "CREATE TRIGGER",
            "CREATE SYNONYM", "CREATE MATERIALIZED VIEW", "UNLIMITED TABLESPACE", "SELECT ANY TABLE", "SELECT ANY DICTIONARY",
        ],
        object_kinds: vec!["", "table", "view", "materialized_view", "procedure", "function", "package", "sequence"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

// -- reading -----------------------------------------------------------------

/// Rows of a catalog query, kept out of the statement cache: the thin
/// client would keep a statement whose parse failed (a DBA_ view without
/// the grant) and report ORA-01003 the next time.
fn rows(c: &Connection, sql: &str, params: &[&dyn ToDbValue]) -> std::result::Result<Vec<Vec<Value>>, oracledb::Error> {
    let stmt = c.statement(sql).map(|b| b.exclude_from_cache()).and_then(|b| b.build())?;
    let cursor = stmt.query(params)?;
    let types: Vec<&'static oracledb::DbType> = cursor.columns().iter().map(|m| m.db_type()).collect();
    let mut out = Vec::new();
    for row in cursor {
        let row = row?;
        out.push(types.iter().enumerate().map(|(i, t)| cell(&row, i, t)).collect());
    }
    Ok(out)
}

fn at(r: &[Value], i: usize) -> Option<String> {
    r.get(i).and_then(txt).filter(|s| !s.is_empty())
}

fn dba(c: &Connection) -> bool {
    rows(c, "SELECT 1 FROM dba_users WHERE ROWNUM = 1", &[]).is_ok() && rows(c, "SELECT 1 FROM dba_role_privs WHERE ROWNUM = 1", &[]).is_ok()
}

const DBA_USERS: &str = "SELECT username, account_status, authentication_type, default_tablespace,
       TO_CHAR(created, 'YYYY-MM-DD HH24:MI'), TO_CHAR(expiry_date, 'YYYY-MM-DD HH24:MI'), oracle_maintained
  FROM dba_users ORDER BY username";
/// 11g: no ORACLE_MAINTAINED.
const DBA_USERS_11G: &str = "SELECT username, account_status, authentication_type, default_tablespace,
       TO_CHAR(created, 'YYYY-MM-DD HH24:MI'), TO_CHAR(expiry_date, 'YYYY-MM-DD HH24:MI'),
       CASE WHEN username IN ('SYS', 'SYSTEM', 'OUTLN', 'DBSNMP', 'XDB', 'APPQOSSYS', 'ANONYMOUS', 'CTXSYS',
                              'MDSYS', 'ORDSYS', 'WMSYS', 'EXFSYS', 'OLAPSYS', 'ORACLE_OCM', 'DIP') THEN 'Y' ELSE 'N' END
  FROM dba_users ORDER BY username";
const DBA_ROLES: &str = "SELECT role, oracle_maintained FROM dba_roles ORDER BY role";
const DBA_ROLES_11G: &str = "SELECT role, 'N' FROM dba_roles ORDER BY role";
const DBA_MEMBERS: &str = "SELECT grantee, granted_role FROM dba_role_privs";

const ALL_USERS: &str = "SELECT username, NULL, NULL, NULL, TO_CHAR(created, 'YYYY-MM-DD HH24:MI'), NULL, oracle_maintained
  FROM all_users ORDER BY username";
const ALL_USERS_11G: &str = "SELECT username, NULL, NULL, NULL, TO_CHAR(created, 'YYYY-MM-DD HH24:MI'), NULL, 'N'
  FROM all_users ORDER BY username";
/// The roles the session's user has, directly or through other roles.
const OWN_ROLES: &str = "SELECT granted_role FROM user_role_privs UNION SELECT granted_role FROM role_role_privs";
const OWN_MEMBERS: &str = "SELECT username, granted_role FROM user_role_privs UNION ALL SELECT role, granted_role FROM role_role_privs";

pub fn principals(c: &Connection) -> Result<Vec<Principal>> {
    let full = dba(c);
    let users = if full {
        rows(c, DBA_USERS, &[]).or_else(|_| rows(c, DBA_USERS_11G, &[]))
    } else {
        rows(c, ALL_USERS, &[]).or_else(|_| rows(c, ALL_USERS_11G, &[]))
    }
    .map_err(err)?;
    let roles: Vec<Vec<Value>> = if full {
        rows(c, DBA_ROLES, &[]).or_else(|_| rows(c, DBA_ROLES_11G, &[])).map_err(err)?
    } else {
        rows(c, OWN_ROLES, &[]).map_err(err)?.into_iter().map(|mut r| {
            r.push(Value::Null);
            r
        }).collect()
    };
    let members = rows(c, if full { DBA_MEMBERS } else { OWN_MEMBERS }, &[]).map_err(err)?;
    let me = rows(c, "SELECT USER FROM dual", &[]).map_err(err)?.first().and_then(|r| at(r, 0)).unwrap_or_default();

    let mut out = Vec::new();
    for r in &users {
        let name = at(r, 0).unwrap_or_default();
        let status = at(r, 1);
        let auth = at(r, 2);
        let mut details = Vec::new();
        if let Some(s) = &status {
            details.push(("Estado".into(), s.clone()));
        }
        if let Some(a) = &auth {
            details.push(("Autenticación".into(), a.clone()));
        }
        if let Some(t) = at(r, 3) {
            details.push(("Tablespace predeterminado".into(), t));
        }
        if let Some(t) = at(r, 4) {
            details.push(("Creado".into(), t));
        }
        if let Some(t) = at(r, 5) {
            details.push(("Vence".into(), t));
        }
        if !full && name == me {
            details.push(("Nota".into(), "Sin acceso a las vistas DBA_: se ven los roles y permisos del usuario propio".into()));
        }
        out.push(Principal {
            kind: PrincipalKind::User,
            // Schema-only accounts (NO AUTHENTICATION) can't sign in.
            can_login: auth.as_deref().map(|a| a != "NONE"),
            superuser: Some(name == "SYS" || name == "SYSTEM"),
            disabled: status.as_deref().map(|s| s.contains("LOCKED")),
            member_of: Vec::new(),
            details,
            system: at(r, 6).as_deref() == Some("Y"),
            name,
        });
    }
    for r in &roles {
        let name = at(r, 0).unwrap_or_default();
        out.push(Principal {
            kind: PrincipalKind::Role,
            superuser: Some(name == "DBA"),
            system: at(r, 1).as_deref() == Some("Y") || name == "PUBLIC",
            name,
            ..Default::default()
        });
    }
    for m in &members {
        let (Some(member), Some(role)) = (at(m, 0), at(m, 1)) else { continue };
        if let Some(p) = out.iter_mut().find(|p| p.name == member) {
            if !p.member_of.contains(&role) {
                if role == "DBA" {
                    p.superuser = Some(true);
                }
                p.member_of.push(role);
            }
        }
    }
    Ok(out)
}

// privilege, object owner, object name, column, grantable, object type
const DBA_GRANTS: &str = "
SELECT privilege, NULL, NULL, NULL, admin_option, NULL FROM dba_sys_privs WHERE grantee = :1
UNION ALL
SELECT p.privilege, p.owner, p.table_name, NULL, p.grantable,
       (SELECT MIN(o.object_type) FROM dba_objects o
         WHERE o.owner = p.owner AND o.object_name = p.table_name AND o.object_type NOT IN ('PACKAGE BODY', 'TYPE BODY'))
  FROM dba_tab_privs p WHERE p.grantee = :2
UNION ALL
SELECT privilege, owner, table_name, column_name, grantable, 'COLUMN' FROM dba_col_privs WHERE grantee = :3";
const DBA_ROLES_OF: &str = "SELECT granted_role FROM dba_role_privs WHERE grantee = :1";

const OWN_GRANTS: &str = "
SELECT privilege, NULL, NULL, NULL, admin_option, NULL FROM user_sys_privs WHERE username = :1
UNION ALL
SELECT privilege, NULL, NULL, NULL, admin_option, NULL FROM role_sys_privs WHERE role = :2
UNION ALL
SELECT p.privilege, p.table_schema, p.table_name, NULL, p.grantable,
       (SELECT MIN(o.object_type) FROM all_objects o
         WHERE o.owner = p.table_schema AND o.object_name = p.table_name AND o.object_type NOT IN ('PACKAGE BODY', 'TYPE BODY'))
  FROM all_tab_privs p WHERE p.grantee = :3
UNION ALL
SELECT privilege, owner, table_name, column_name, grantable, 'COLUMN' FROM role_tab_privs WHERE role = :4 AND column_name IS NOT NULL
UNION ALL
SELECT p.privilege, p.owner, p.table_name, NULL, p.grantable,
       (SELECT MIN(o.object_type) FROM all_objects o
         WHERE o.owner = p.owner AND o.object_name = p.table_name AND o.object_type NOT IN ('PACKAGE BODY', 'TYPE BODY'))
  FROM role_tab_privs p WHERE p.role = :5 AND p.column_name IS NULL
UNION ALL
SELECT privilege, table_schema, table_name, column_name, grantable, 'COLUMN' FROM all_col_privs WHERE grantee = :6";
const OWN_ROLES_OF: &str = "SELECT granted_role FROM user_role_privs WHERE username = :1
UNION SELECT granted_role FROM role_role_privs WHERE role = :2";

fn kind_of(object_type: &str) -> &'static str {
    match object_type {
        "VIEW" => "view",
        "MATERIALIZED VIEW" => "materialized_view",
        "PROCEDURE" => "procedure",
        "FUNCTION" => "function",
        "PACKAGE" => "package",
        "SEQUENCE" => "sequence",
        "TYPE" => "type",
        "DIRECTORY" => "directory",
        _ => "table",
    }
}

pub fn grants(c: &Connection, principal: &str) -> Result<Vec<Grant>> {
    let full = dba(c);
    let (grants_sql, roles_sql) = if full { (DBA_GRANTS, DBA_ROLES_OF) } else { (OWN_GRANTS, OWN_ROLES_OF) };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::from([(principal.to_string(), None)]);
    while let Some((name, via)) = queue.pop_front() {
        if !seen.insert(name.clone()) || seen.len() > 256 {
            continue;
        }
        // One bind per placeholder (:1, :2…): SQL binds by position.
        let binds = |sql: &str| vec![&name as &dyn ToDbValue; sql.matches(':').count()];
        for r in rows(c, grants_sql, &binds(grants_sql)).map_err(err)? {
            let privilege = at(&r, 0).unwrap_or_default();
            let ty = at(&r, 5).unwrap_or_default();
            let (privilege, object, object_kind) = match (at(&r, 1), at(&r, 2)) {
                (Some(owner), Some(obj)) => match at(&r, 3) {
                    Some(col) => (format!("{privilege} ({col})"), Some(format!("{owner}.{obj}")), Some("table".to_string())),
                    None => (privilege, Some(format!("{owner}.{obj}")), Some(kind_of(&ty).to_string())),
                },
                _ => (privilege, None, None),
            };
            out.push(Grant { privilege, object, object_kind, grantable: at(&r, 4).as_deref() == Some("YES"), denied: false, via: via.clone() });
        }
        for r in rows(c, roles_sql, &binds(roles_sql)).map_err(err)? {
            if let Some(role) = at(&r, 0) {
                let v = via.clone().unwrap_or_else(|| role.clone());
                queue.push_back((role, Some(v)));
            }
        }
    }
    Ok(out)
}

// -- scripts -----------------------------------------------------------------

/// A password as Oracle takes it: a quoted identifier, so it can't have `"`.
fn password(p: &str) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("escribí la contraseña del usuario".into()));
    }
    if p.contains('"') {
        return Err(Error::Query("en Oracle la contraseña no puede tener comillas dobles (\")".into()));
    }
    Ok(format!("\"{p}\""))
}

fn on(o: &ObjectRef) -> String {
    match o.schema() {
        Some(sc) => format!("{}.{}", q(sc), q(&o.name)),
        None => q(&o.name),
    }
}

/// Privilege names: letters, spaces and underscores, optionally with a
/// column list (`UPDATE (a, b)`). `columns: false` drops the list (Oracle
/// revokes a column privilege on every column).
fn privileges(p: &[String], columns: bool) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let bad = |x: &str| Error::Query(format!("«{x}» no es un permiso de Oracle"));
    let mut out = Vec::new();
    for x in p {
        let (name, cols) = match x.split_once('(') {
            Some((n, c)) => (n.trim(), Some(c.trim().strip_suffix(')').ok_or_else(|| bad(x))?)),
            None => (x.trim(), None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '_') {
            return Err(bad(x));
        }
        let name = name.to_uppercase();
        match cols.filter(|_| columns) {
            Some(c) => {
                let cols: Vec<String> = c.split(',').map(|c| c.trim().trim_matches('"')).filter(|c| !c.is_empty()).map(q).collect();
                if cols.is_empty() {
                    return Err(bad(x));
                }
                out.push(format!("{name} ({})", cols.join(", ")));
            }
            None => {
                if !out.contains(&name) {
                    out.push(name)
                }
            }
        }
    }
    Ok(out.join(", "))
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => {
            let name = q(&schema_name(name)?);
            format!(
                "CREATE USER {name} IDENTIFIED BY {};\n-- Sin CREATE SESSION no puede conectarse.\nGRANT CREATE SESSION TO {name};",
                password(pw.as_deref().unwrap_or_default())?
            )
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", q(&schema_name(name)?)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!(
            "DROP USER {n};\n-- Si el usuario tiene objetos, Oracle exige CASCADE, que los borra junto con él:\n-- DROP USER {n} CASCADE;",
            n = q(name)
        ),
        SecurityAction::SetPassword { name, password: pw } => format!("ALTER USER {} IDENTIFIED BY {};", q(name), password(pw)?),
        SecurityAction::SetLogin { name, enabled } => {
            format!("ALTER USER {} ACCOUNT {};", q(name), if *enabled { "UNLOCK" } else { "LOCK" })
        }
        SecurityAction::Grant { privileges: p, object: None, to, grantable } => {
            format!("GRANT {} TO {}{};", privileges(p, false)?, q(to), if *grantable { " WITH ADMIN OPTION" } else { "" })
        }
        SecurityAction::Grant { privileges: p, object: Some(o), to, grantable } => {
            format!("GRANT {} ON {} TO {}{};", privileges(p, true)?, on(o), q(to), if *grantable { " WITH GRANT OPTION" } else { "" })
        }
        SecurityAction::Revoke { privileges: p, object: None, from } => format!("REVOKE {} FROM {};", privileges(p, false)?, q(from)),
        SecurityAction::Revoke { privileges: p, object: Some(o), from } => {
            format!("REVOKE {} ON {} FROM {};", privileges(p, false)?, on(o), q(from))
        }
        SecurityAction::AddMember { role, member } => format!("GRANT {} TO {};", q(role), q(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE {} FROM {};", q(role), q(member)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w".into()) }),
            "CREATE USER \"ANA\" IDENTIFIED BY \"p'w\";\n-- Sin CREATE SESSION no puede conectarse.\nGRANT CREATE SESSION TO \"ANA\";"
        );
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: Some("a\"b".into()) }).is_err());
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "Mixed Name".into() }), "CREATE ROLE \"Mixed Name\";");
        assert!(s(SecurityAction::Drop { name: "ANA".into(), kind: PrincipalKind::User }).starts_with("DROP USER \"ANA\";\n-- "));
        assert_eq!(s(SecurityAction::SetLogin { name: "ANA".into(), enabled: false }), "ALTER USER \"ANA\" ACCOUNT LOCK;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["create table".into()], object: None, to: "ANA".into(), grantable: true }),
            "GRANT CREATE TABLE TO \"ANA\" WITH ADMIN OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["SELECT".into(), "UPDATE (TOTAL)".into()],
                object: Some(ObjectRef { kind: "table".into(), schema: Some("VENTAS".into()), name: "FACTURAS".into() }),
                to: "ANA".into(),
                grantable: true,
            }),
            "GRANT SELECT, UPDATE (\"TOTAL\") ON \"VENTAS\".\"FACTURAS\" TO \"ANA\" WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Revoke {
                privileges: vec!["UPDATE (TOTAL)".into()],
                object: Some(ObjectRef { kind: "table".into(), schema: Some("VENTAS".into()), name: "FACTURAS".into() }),
                from: "ANA".into(),
            }),
            "REVOKE UPDATE ON \"VENTAS\".\"FACTURAS\" FROM \"ANA\";"
        );
        assert_eq!(s(SecurityAction::AddMember { role: "LECT".into(), member: "ANA".into() }), "GRANT \"LECT\" TO \"ANA\";");
        assert_eq!(s(SecurityAction::RemoveMember { role: "LECT".into(), member: "ANA".into() }), "REVOKE \"LECT\" FROM \"ANA\";");
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT; DROP TABLE x".into()], object: None, to: "a".into(), grantable: false }).is_err());
    }
}

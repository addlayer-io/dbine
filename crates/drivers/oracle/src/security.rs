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
use dbine_driver::{
    Error, Grant, ObjectRef, Principal, PrincipalKind, Privilege, Result, SecurityAction,
    SecuritySpec,
};
use oracledb::{Connection, ToDbValue};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            // On objects.
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "EXECUTE",
            "REFERENCES",
            "ALTER",
            "INDEX",
            "READ",
            "DEBUG",
            // System-wide (granted without an object).
            "CREATE SESSION",
            "CREATE TABLE",
            "CREATE VIEW",
            "CREATE SEQUENCE",
            "CREATE PROCEDURE",
            "CREATE TRIGGER",
            "CREATE SYNONYM",
            "CREATE MATERIALIZED VIEW",
            "UNLIMITED TABLESPACE",
            "SELECT ANY TABLE",
            "SELECT ANY DICTIONARY",
        ],
        object_kinds: vec![
            "",
            "table",
            "view",
            "materialized_view",
            "procedure",
            "function",
            "package",
            "sequence",
        ],
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
fn rows(
    c: &Connection,
    sql: &str,
    params: &[&dyn ToDbValue],
) -> std::result::Result<Vec<Vec<Value>>, oracledb::Error> {
    let stmt = c
        .statement(sql)
        .map(|b| b.exclude_from_cache())
        .and_then(|b| b.build())?;
    let cursor = stmt.query(params)?;
    let types: Vec<&'static oracledb::DbType> =
        cursor.columns().iter().map(|m| m.db_type()).collect();
    let mut out = Vec::new();
    for row in cursor {
        let row = row?;
        out.push(
            types
                .iter()
                .enumerate()
                .map(|(i, t)| cell(&row, i, t))
                .collect(),
        );
    }
    Ok(out)
}

fn at(r: &[Value], i: usize) -> Option<String> {
    r.get(i).and_then(txt).filter(|s| !s.is_empty())
}

fn dba(c: &Connection) -> bool {
    rows(c, "SELECT 1 FROM dba_users WHERE ROWNUM = 1", &[]).is_ok()
        && rows(c, "SELECT 1 FROM dba_role_privs WHERE ROWNUM = 1", &[]).is_ok()
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
const ALL_USERS_11G: &str =
    "SELECT username, NULL, NULL, NULL, TO_CHAR(created, 'YYYY-MM-DD HH24:MI'), NULL, 'N'
  FROM all_users ORDER BY username";
/// The roles the session's user has, directly or through other roles.
const OWN_ROLES: &str =
    "SELECT granted_role FROM user_role_privs UNION SELECT granted_role FROM role_role_privs";
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
        rows(c, DBA_ROLES, &[])
            .or_else(|_| rows(c, DBA_ROLES_11G, &[]))
            .map_err(err)?
    } else {
        rows(c, OWN_ROLES, &[])
            .map_err(err)?
            .into_iter()
            .map(|mut r| {
                r.push(Value::Null);
                r
            })
            .collect()
    };
    let members = rows(c, if full { DBA_MEMBERS } else { OWN_MEMBERS }, &[]).map_err(err)?;
    let me = rows(c, "SELECT USER FROM dual", &[])
        .map_err(err)?
        .first()
        .and_then(|r| at(r, 0))
        .unwrap_or_default();

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
            details.push((
                "Nota".into(),
                "Sin acceso a las vistas DBA_: se ven los roles y permisos del usuario propio"
                    .into(),
            ));
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
        let (Some(member), Some(role)) = (at(m, 0), at(m, 1)) else {
            continue;
        };
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
    let (grants_sql, roles_sql) = if full {
        (DBA_GRANTS, DBA_ROLES_OF)
    } else {
        (OWN_GRANTS, OWN_ROLES_OF)
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> =
        VecDeque::from([(principal.to_string(), None)]);
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
                    Some(col) => (
                        format!("{privilege} ({col})"),
                        Some(format!("{owner}.{obj}")),
                        Some("table".to_string()),
                    ),
                    None => (
                        privilege,
                        Some(format!("{owner}.{obj}")),
                        Some(kind_of(&ty).to_string()),
                    ),
                },
                _ => (privilege, None, None),
            };
            out.push(Grant {
                privilege,
                object,
                object_kind,
                grantable: at(&r, 4).as_deref() == Some("YES"),
                denied: false,
                via: via.clone(),
            });
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

// -- what can be granted ---------------------------------------------------

const SYSTEM_PRIVILEGE_MAP: &str = "SELECT name FROM system_privilege_map ORDER BY name";
const TABLE_PRIVILEGE_MAP: &str = "SELECT name FROM table_privilege_map";

const TABLES: &[&str] = &["table", "view", "materialized_view"];

/// The object privileges of TABLE_PRIVILEGE_MAP and the spec's kinds GRANT
/// takes them on (the map doesn't say), in the order the tab offers them:
/// "Object Privileges" in
/// https://docs.oracle.com/en/database/oracle/oracle-database/23/sqlrf/GRANT.html,
/// checked one by one on 23ai. Granting on a materialized view grants on
/// its table. INDEX, REFERENCES, ASSERTION REFERENCES and NOTIFICATION
/// DIRECTIVE go to users only, not to roles.
const OBJECT_PRIVILEGES: &[(&str, &[&str])] = &[
    (
        "SELECT",
        &["table", "view", "materialized_view", "sequence"],
    ),
    ("INSERT", TABLES),
    ("UPDATE", TABLES),
    ("DELETE", TABLES),
    ("READ", TABLES),
    ("EXECUTE", &["procedure", "function", "package"]),
    ("REFERENCES", TABLES),
    ("ALTER", &["table", "materialized_view", "sequence"]),
    ("INDEX", &["table", "materialized_view"]),
    (
        "DEBUG",
        &[
            "table",
            "view",
            "materialized_view",
            "procedure",
            "function",
            "package",
        ],
    ),
    ("FLASHBACK", TABLES),
    ("ON COMMIT REFRESH", TABLES),
    ("QUERY REWRITE", TABLES),
    ("MERGE VIEW", &["view"]),
    // Object views (and types).
    ("UNDER", &["view"]),
    ("KEEP SEQUENCE", &["sequence"]),
    // Blockchain and immutable tables.
    ("SIGN", &["table"]),
    ("ASSERTION REFERENCES", &["table", "materialized_view"]),
    ("NOTIFICATION DIRECTIVE", TABLES),
    ("VALIDATION DIRECTIVE", TABLES),
    // On kinds the tab doesn't offer: directories (WRITE), editions and SQL
    // translation profiles (USE), flashback archives. 23ai takes COLLECT
    // DEBUG INFO on none of the spec's kinds.
    ("WRITE", &[]),
    ("USE", &[]),
    ("FLASHBACK ARCHIVE", &[]),
    ("COLLECT DEBUG INFO", &[]),
    // Not with GRANT (ORA-00990): queues go through DBMS_AQADM; the rest are
    // the map's old entries.
    ("ENQUEUE", &[]),
    ("DEQUEUE", &[]),
    ("AUDIT", &[]),
    ("COMMENT", &[]),
    ("GRANT", &[]),
    ("LOCK", &[]),
    ("RENAME", &[]),
    ("CREATE", &[]),
];

/// SYSRAC is in the map, but GRANT refuses it (ORA-28190: it's for the
/// clusterware agent).
const NOT_GRANTABLE: &[&str] = &["SYSRAC"];

/// System and administrative privileges, granted without an object, for when
/// SYSTEM_PRIVILEGE_MAP can't be read: Oracle 23ai's (the "System
/// Privileges" and "Administrative Privileges" tables of GRANT, linked
/// above).
const SYSTEM_PRIVILEGES: &str = "\
ADMINISTER ANY DATA GRANT
ADMINISTER ANY SQL TUNING SET
ADMINISTER DATABASE TRIGGER
ADMINISTER FINE GRAINED AUDIT POLICY
ADMINISTER KEY MANAGEMENT
ADMINISTER REDACTION POLICY
ADMINISTER RESOURCE MANAGER
ADMINISTER ROW LEVEL SECURITY POLICY
ADMINISTER SQL FIREWALL
ADMINISTER SQL MANAGEMENT OBJECT
ADMINISTER SQL TUNING SET
ADVISOR
ALTER ANY ANALYTIC VIEW
ALTER ANY ASSEMBLY
ALTER ANY ASSERTION
ALTER ANY ATTRIBUTE DIMENSION
ALTER ANY CLUSTER
ALTER ANY CUBE
ALTER ANY CUBE BUILD PROCESS
ALTER ANY CUBE DIMENSION
ALTER ANY DIMENSION
ALTER ANY DOMAIN
ALTER ANY EDITION
ALTER ANY EVALUATION CONTEXT
ALTER ANY HIERARCHY
ALTER ANY INDEX
ALTER ANY INDEXTYPE
ALTER ANY LIBRARY
ALTER ANY MATERIALIZED VIEW
ALTER ANY MEASURE FOLDER
ALTER ANY MINING MODEL
ALTER ANY MLE
ALTER ANY NOTIFICATION DIRECTIVE
ALTER ANY OPERATOR
ALTER ANY OUTLINE
ALTER ANY PROCEDURE
ALTER ANY PROPERTY GRAPH
ALTER ANY ROLE
ALTER ANY RULE
ALTER ANY RULE SET
ALTER ANY SEQUENCE
ALTER ANY SQL PROFILE
ALTER ANY SQL TRANSLATION PROFILE
ALTER ANY TABLE
ALTER ANY TRIGGER
ALTER ANY TYPE
ALTER ANY VALIDATION DIRECTIVE
ALTER DATABASE
ALTER DATABASE LINK
ALTER END USER
ALTER LOCKDOWN PROFILE
ALTER PROFILE
ALTER PUBLIC DATABASE LINK
ALTER RESOURCE COST
ALTER ROLLBACK SEGMENT
ALTER SESSION
ALTER SYSTEM
ALTER TABLESPACE
ALTER USER
ANALYZE ANY
ANALYZE ANY DICTIONARY
AUDIT ANY
AUDIT SYSTEM
BACKUP ANY TABLE
BECOME USER
CHANGE NOTIFICATION
CLEAR ALL LOCAL CONTEXTS
COMMENT ANY MINING MODEL
COMMENT ANY PROPERTY GRAPH
COMMENT ANY TABLE
CREATE ANALYTIC VIEW
CREATE ANY ANALYTIC VIEW
CREATE ANY ASSEMBLY
CREATE ANY ASSERTION
CREATE ANY ATTRIBUTE DIMENSION
CREATE ANY CLUSTER
CREATE ANY CONTEXT
CREATE ANY CREDENTIAL
CREATE ANY CUBE
CREATE ANY CUBE BUILD PROCESS
CREATE ANY CUBE DIMENSION
CREATE ANY DATA GRANT
CREATE ANY DATA PRIVILEGE
CREATE ANY DIMENSION
CREATE ANY DIRECTORY
CREATE ANY DOMAIN
CREATE ANY EDITION
CREATE ANY END USER CONTEXT
CREATE ANY EVALUATION CONTEXT
CREATE ANY HIERARCHY
CREATE ANY INDEX
CREATE ANY INDEXTYPE
CREATE ANY JOB
CREATE ANY LIBRARY
CREATE ANY MATERIALIZED VIEW
CREATE ANY MEASURE FOLDER
CREATE ANY MINING MODEL
CREATE ANY MLE
CREATE ANY NOTIFICATION DIRECTIVE
CREATE ANY OPERATOR
CREATE ANY OUTLINE
CREATE ANY PROCEDURE
CREATE ANY PROPERTY GRAPH
CREATE ANY RULE
CREATE ANY RULE SET
CREATE ANY SEQUENCE
CREATE ANY SQL PROFILE
CREATE ANY SQL TRANSLATION PROFILE
CREATE ANY SYNONYM
CREATE ANY TABLE
CREATE ANY TRIGGER
CREATE ANY TYPE
CREATE ANY VALIDATION DIRECTIVE
CREATE ANY VIEW
CREATE APPLICATION IDENTITY
CREATE ASSEMBLY
CREATE ASSERTION
CREATE ATTRIBUTE DIMENSION
CREATE CLUSTER
CREATE CREDENTIAL
CREATE CUBE
CREATE CUBE BUILD PROCESS
CREATE CUBE DIMENSION
CREATE DATA GRANT
CREATE DATA PRIVILEGE
CREATE DATA ROLE
CREATE DATABASE LINK
CREATE DIMENSION
CREATE DOMAIN
CREATE END USER
CREATE END USER CONTEXT
CREATE END USER SECURITY CONTEXT
CREATE EVALUATION CONTEXT
CREATE EXTERNAL JOB
CREATE HIERARCHY
CREATE INDEXTYPE
CREATE JOB
CREATE LIBRARY
CREATE LOCKDOWN PROFILE
CREATE LOGICAL PARTITION TRACKING
CREATE MATERIALIZED VIEW
CREATE MEASURE FOLDER
CREATE MINING MODEL
CREATE MLE
CREATE NOTIFICATION DIRECTIVE
CREATE OPERATOR
CREATE PLUGGABLE DATABASE
CREATE PROCEDURE
CREATE PROFILE
CREATE PROPERTY GRAPH
CREATE PUBLIC DATABASE LINK
CREATE PUBLIC SYNONYM
CREATE ROLE
CREATE ROLLBACK SEGMENT
CREATE RULE
CREATE RULE SET
CREATE SEQUENCE
CREATE SESSION
CREATE SQL TRANSLATION PROFILE
CREATE SYNONYM
CREATE TABLE
CREATE TABLESPACE
CREATE TRIGGER
CREATE TRUE CACHE
CREATE TYPE
CREATE USER
CREATE VIEW
DEBUG ANY PROCEDURE
DEBUG CONNECT ANY
DEBUG CONNECT SESSION
DELETE ANY CUBE DIMENSION
DELETE ANY MEASURE FOLDER
DELETE ANY TABLE
DEQUEUE ANY QUEUE
DROP ANY ANALYTIC VIEW
DROP ANY ASSEMBLY
DROP ANY ASSERTION
DROP ANY ATTRIBUTE DIMENSION
DROP ANY CLUSTER
DROP ANY CONTEXT
DROP ANY CUBE
DROP ANY CUBE BUILD PROCESS
DROP ANY CUBE DIMENSION
DROP ANY DATA GRANT
DROP ANY DATA PRIVILEGE
DROP ANY DIMENSION
DROP ANY DIRECTORY
DROP ANY DOMAIN
DROP ANY EDITION
DROP ANY END USER CONTEXT
DROP ANY EVALUATION CONTEXT
DROP ANY HIERARCHY
DROP ANY INDEX
DROP ANY INDEXTYPE
DROP ANY LIBRARY
DROP ANY MATERIALIZED VIEW
DROP ANY MEASURE FOLDER
DROP ANY MINING MODEL
DROP ANY MLE
DROP ANY NOTIFICATION DIRECTIVE
DROP ANY OPERATOR
DROP ANY OUTLINE
DROP ANY PROCEDURE
DROP ANY PROPERTY GRAPH
DROP ANY ROLE
DROP ANY RULE
DROP ANY RULE SET
DROP ANY SEQUENCE
DROP ANY SQL PROFILE
DROP ANY SQL TRANSLATION PROFILE
DROP ANY SYNONYM
DROP ANY TABLE
DROP ANY TRIGGER
DROP ANY TYPE
DROP ANY VALIDATION DIRECTIVE
DROP ANY VIEW
DROP APPLICATION IDENTITY
DROP DATA ROLE
DROP END USER
DROP LOCKDOWN PROFILE
DROP LOGICAL PARTITION TRACKING
DROP PROFILE
DROP PUBLIC DATABASE LINK
DROP PUBLIC SYNONYM
DROP ROLLBACK SEGMENT
DROP TABLESPACE
DROP USER
ENABLE DIAGNOSTICS
ENQUEUE ANY QUEUE
EXECUTE ANY ASSEMBLY
EXECUTE ANY CLASS
EXECUTE ANY DOMAIN
EXECUTE ANY EVALUATION CONTEXT
EXECUTE ANY INDEXTYPE
EXECUTE ANY LIBRARY
EXECUTE ANY OPERATOR
EXECUTE ANY PROCEDURE
EXECUTE ANY PROGRAM
EXECUTE ANY RULE
EXECUTE ANY RULE SET
EXECUTE ANY TYPE
EXECUTE ASSEMBLY
EXECUTE DYNAMIC MLE
EXEMPT ACCESS POLICY
EXEMPT IDENTITY POLICY
EXEMPT REDACTION POLICY
EXPORT FULL DATABASE
FLASHBACK ANY TABLE
FLASHBACK ARCHIVE ADMINISTER
FORCE ANY TRANSACTION
FORCE TRANSACTION
GLOBAL QUERY REWRITE
GRANT ANY DATA ROLE
GRANT ANY OBJECT PRIVILEGE
GRANT ANY PRIVILEGE
GRANT ANY ROLE
GRANT ANY SCHEMA PRIVILEGE
IMPORT FULL DATABASE
INHERIT ANY PRIVILEGES
INHERIT ANY REMOTE PRIVILEGES
INSERT ANY CUBE DIMENSION
INSERT ANY MEASURE FOLDER
INSERT ANY TABLE
KEEP DATE TIME
KEEP SYSGUID
LOCK ANY TABLE
LOGMINING
MANAGE ANY QUEUE
MANAGE SCHEDULER
MANAGE TABLESPACE
MERGE ANY VIEW
ON COMMIT REFRESH
PURGE DBA_RECYCLEBIN
QUERY REWRITE
READ ANY ANALYTIC VIEW CACHE
READ ANY PROPERTY GRAPH
READ ANY TABLE
REDEFINE ANY TABLE
RESTRICTED SESSION
RESUMABLE
SELECT ANY CUBE
SELECT ANY CUBE BUILD PROCESS
SELECT ANY CUBE DIMENSION
SELECT ANY DICTIONARY
SELECT ANY END USER CONTEXT
SELECT ANY MEASURE FOLDER
SELECT ANY MINING MODEL
SELECT ANY SEQUENCE
SELECT ANY TABLE
SELECT ANY TRANSACTION
SET CONTAINER
SET USE DATA GRANTS ONLY
SYSBACKUP
SYSDBA
SYSDG
SYSKM
SYSOPER
TABLE RETENTION
TEXT DATASTORE ACCESS
TRANSLATE ANY SQL
UNDER ANY TABLE
UNDER ANY TYPE
UNDER ANY VIEW
UNLIMITED TABLESPACE
UPDATE ANY CUBE
UPDATE ANY CUBE BUILD PROCESS
UPDATE ANY CUBE DIMENSION
UPDATE ANY END USER CONTEXT
UPDATE ANY TABLE
USE ANY JOB RESOURCE
USE ANY SQL TRANSLATION PROFILE
WRITE ANY ANALYTIC VIEW CACHE";

/// The privileges this server can grant: its SYSTEM_PRIVILEGE_MAP and
/// TABLE_PRIVILEGE_MAP (granted to PUBLIC), so they follow its version; the
/// 23ai ones if those can't be read.
pub fn available(c: &Connection) -> Result<Vec<Privilege>> {
    let names = |sql| -> Option<Vec<String>> {
        let rows = rows(c, sql, &[]).ok()?;
        let names: Vec<String> = rows.iter().filter_map(|r| at(r, 0)).collect();
        (!names.is_empty()).then_some(names)
    };
    Ok(
        match (names(TABLE_PRIVILEGE_MAP), names(SYSTEM_PRIVILEGE_MAP)) {
            (Some(object), Some(system)) => listed(&object, &system),
            _ => static_privileges(),
        },
    )
}

/// The catalog's names as privileges: object privileges on their kinds (one
/// this crate doesn't know on every object kind), system ones on "".
fn listed(object: &[String], system: &[String]) -> Vec<Privilege> {
    let mut out: Vec<Privilege> = OBJECT_PRIVILEGES
        .iter()
        .filter(|(name, on)| !on.is_empty() && object.iter().any(|o| o == name))
        .map(|(name, on)| Privilege::new(*name, on))
        .collect();
    let kinds: Vec<&str> = spec()
        .object_kinds
        .into_iter()
        .filter(|k| !k.is_empty())
        .collect();
    for name in object {
        if !OBJECT_PRIVILEGES.iter().any(|(n, _)| n == name) && valid(name) {
            out.push(Privilege::new(name.clone(), &kinds));
        }
    }
    for name in system {
        if !NOT_GRANTABLE.contains(&name.as_str()) && valid(name) {
            out.push(Privilege::new(name.clone(), &[""]));
        }
    }
    out
}

fn static_privileges() -> Vec<Privilege> {
    OBJECT_PRIVILEGES
        .iter()
        .filter(|(_, on)| !on.is_empty())
        .map(|(name, on)| Privilege::new(*name, on))
        .chain(SYSTEM_PRIVILEGES.lines().map(|n| Privilege::new(n, &[""])))
        .collect()
}

// -- scripts -----------------------------------------------------------------

/// A password as Oracle takes it: a quoted identifier, so it can't have `"`.
fn password(p: &str) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("escribí la contraseña del usuario".into()));
    }
    if p.contains('"') {
        return Err(Error::Query(
            "en Oracle la contraseña no puede tener comillas dobles (\")".into(),
        ));
    }
    Ok(format!("\"{p}\""))
}

fn on(o: &ObjectRef) -> String {
    match o.schema() {
        Some(sc) => format!("{}.{}", q(sc), q(&o.name)),
        None => q(&o.name),
    }
}

/// A privilege's name: letters, spaces and underscores.
fn valid(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '_')
}

/// Privilege names (see [`valid`]), optionally with a column list
/// (`UPDATE (a, b)`). `columns: false` drops the list (Oracle revokes a
/// column privilege on every column).
fn privileges(p: &[String], columns: bool) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let bad = |x: &str| Error::Query(format!("«{x}» no es un permiso de Oracle"));
    let mut out = Vec::new();
    for x in p {
        let (name, cols) = match x.split_once('(') {
            Some((n, c)) => (
                n.trim(),
                Some(c.trim().strip_suffix(')').ok_or_else(|| bad(x))?),
            ),
            None => (x.trim(), None),
        };
        if !valid(name) {
            return Err(bad(x));
        }
        let name = name.to_uppercase();
        match cols.filter(|_| columns) {
            Some(c) => {
                let cols: Vec<String> = c
                    .split(',')
                    .map(|c| c.trim().trim_matches('"'))
                    .filter(|c| !c.is_empty())
                    .map(q)
                    .collect();
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
            "DROP USER {n};\n-- Si el usuario tiene objetos, Oracle exige CASCADE, que los borra junto con él:\n-- DROP USER {c} CASCADE;",
            n = q(name),
            c = comment_text(&q(name))
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

/// Text for a `--` comment line: a server-controlled name can't end the
/// comment and turn the rest of the line into a statement. Line breaks
/// (CR, LF, NEL, U+2028/U+2029) and other control characters become `?`.
fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                '?'
            } else {
                c
            }
        })
        .collect()
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
        assert!(script(&SecurityAction::CreateUser {
            name: "ana".into(),
            password: Some("a\"b".into())
        })
        .is_err());
        assert!(script(&SecurityAction::CreateUser {
            name: "ana".into(),
            password: None
        })
        .is_err());
        assert_eq!(
            s(SecurityAction::CreateRole {
                name: "Mixed Name".into()
            }),
            "CREATE ROLE \"Mixed Name\";"
        );
        assert!(s(SecurityAction::Drop {
            name: "ANA".into(),
            kind: PrincipalKind::User
        })
        .starts_with("DROP USER \"ANA\";\n-- "));
        assert_eq!(
            s(SecurityAction::SetLogin {
                name: "ANA".into(),
                enabled: false
            }),
            "ALTER USER \"ANA\" ACCOUNT LOCK;"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["create table".into()],
                object: None,
                to: "ANA".into(),
                grantable: true
            }),
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
                object: Some(ObjectRef {
                    kind: "table".into(),
                    schema: Some("VENTAS".into()),
                    name: "FACTURAS".into()
                }),
                from: "ANA".into(),
            }),
            "REVOKE UPDATE ON \"VENTAS\".\"FACTURAS\" FROM \"ANA\";"
        );
        assert_eq!(
            s(SecurityAction::AddMember {
                role: "LECT".into(),
                member: "ANA".into()
            }),
            "GRANT \"LECT\" TO \"ANA\";"
        );
        assert_eq!(
            s(SecurityAction::RemoveMember {
                role: "LECT".into(),
                member: "ANA".into()
            }),
            "REVOKE \"LECT\" FROM \"ANA\";"
        );
        assert!(script(&SecurityAction::Grant {
            privileges: vec!["SELECT; DROP TABLE x".into()],
            object: None,
            to: "a".into(),
            grantable: false
        })
        .is_err());
    }

    /// Every privilege, on each of its kinds, makes a GRANT script.
    fn grantable(list: &[Privilege]) {
        let kinds = spec().object_kinds;
        for p in list {
            assert!(!p.on.is_empty(), "{}", p.name);
            for k in &p.on {
                assert!(kinds.contains(&k.as_str()), "{} on {k}", p.name);
                let object = (!k.is_empty()).then(|| ObjectRef {
                    kind: k.clone(),
                    schema: Some("VENTAS".into()),
                    name: "OBJ".into(),
                });
                let s = script(&SecurityAction::Grant {
                    privileges: vec![p.name.clone()],
                    object,
                    to: "ANA".into(),
                    grantable: false,
                })
                .unwrap_or_else(|e| panic!("{} on {k}: {e}", p.name));
                assert!(s.starts_with(&format!("GRANT {} ", p.name)), "{s}");
            }
        }
    }

    #[test]
    fn static_privileges_make_scripts() {
        let list = static_privileges();
        assert!(list.len() > 300);
        grantable(&list);
        let on = |n: &str| list.iter().find(|p| p.name == n).map(|p| p.on.clone());
        assert_eq!(on("CREATE SESSION"), Some(vec![String::new()]));
        assert_eq!(
            on("EXECUTE"),
            Some(vec![
                "procedure".into(),
                "function".into(),
                "package".into()
            ])
        );
        assert_eq!(on("SYSRAC"), None);
        assert_eq!(on("ENQUEUE"), None);
    }

    #[test]
    fn maps_the_catalog() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let list = listed(
            &names(&[
                "UPDATE",
                "SELECT",
                "WRITE",
                "AUDIT",
                "NEW THING",
                "BAD;NAME",
            ]),
            &names(&["CREATE SESSION", "SYSRAC", "SELECT ANY TABLE"]),
        );
        grantable(&list);
        let got: Vec<(&str, Vec<&str>)> = list
            .iter()
            .map(|p| (p.name.as_str(), p.on.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "SELECT",
                    vec!["table", "view", "materialized_view", "sequence"]
                ),
                ("UPDATE", vec!["table", "view", "materialized_view"]),
                (
                    "NEW THING",
                    vec![
                        "table",
                        "view",
                        "materialized_view",
                        "procedure",
                        "function",
                        "package",
                        "sequence"
                    ]
                ),
                ("CREATE SESSION", vec![""]),
                ("SELECT ANY TABLE", vec![""]),
            ]
        );
    }
}

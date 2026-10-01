//! Users, roles and permissions (docs/usuarios-y-permisos.md) for the ODBC
//! presets whose SQL has them and whose catalog DBine knows:
//!
//! - **Hive** (and Cloudera CDP's Hive), with SQL standard based
//!   authorization or Ranger: `SHOW ROLES`, `SHOW PRINCIPALS`,
//!   `SHOW GRANT … ON ALL`, `CREATE ROLE`, `GRANT … TO USER|ROLE`. Users
//!   come from Kerberos/LDAP: they're listed as the roles' members.
//! - **Impala** (Sentry or Ranger): roles, their grants (`SHOW GRANT ROLE`)
//!   and their groups (`GRANT ROLE r TO GROUP g`); a user's own grants with
//!   Ranger (`SHOW GRANT USER`).
//! - **Vertica**: `v_catalog.users`, `roles` and `grants`; users with
//!   passwords, lock and roles.
//! - **Exasol**: `EXA_DBA_USERS`, `EXA_DBA_ROLES`, `EXA_DBA_ROLE_PRIVS`,
//!   `EXA_DBA_SYS_PRIVS` and `EXA_DBA_OBJ_PRIVS`.
//! - **Db2 for LUW**: `SYSCAT.DBAUTH`, `SCHEMAAUTH`, `TABAUTH`, `ROLES` and
//!   `ROLEAUTH`. Users are the operating system's or LDAP's.
//!
//! - The rest of the presets that have them, one module each under
//!   `security/`: Teradata, SAP ASE and SQL Anywhere, Informix and GBase 8s,
//!   Netezza, Db2 for i and for z/OS, and the other engines listed in
//!   [`dialect`].
//!
//! Hive, Impala and the engines whose GRANT names the grantee's kind
//! (`TO ROLE r`, `TO GROUP g`) get their roles named `role:<name>`; the
//! others take either name as is. [`unsupported`] says why a preset has none.

use crate::presets::Preset;
use crate::OdbcSession;
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use std::collections::{HashMap, HashSet, VecDeque};

mod groups;
mod ibm;
mod informix;
mod misc;
mod netezza;
mod oracle_like;
mod sqlstd;
mod sybase;
mod teradata;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Hive,
    Impala,
    Vertica,
    Exasol,
    Db2,
    Teradata,
    Informix,
    Db2i,
    Db2z,
    Ase,
    SqlAnywhere,
    Netezza,
    Altibase,
    Dameng,
    Cubrid,
    Zen,
    Mimer,
    MonetDb,
    Ingres,
    Iris,
    MaxDb,
    NuoDb,
    HeavyDb,
    Sqream,
    Virtuoso,
    OpenEdge,
    Machbase,
    Ignite,
    Ocient,
}

pub fn dialect(p: &Preset) -> Option<Dialect> {
    Some(match p.id {
        "hive" | "cloudera" => Dialect::Hive,
        "impala" => Dialect::Impala,
        "vertica" => Dialect::Vertica,
        "exasol" => Dialect::Exasol,
        "db2" => Dialect::Db2,
        "teradata" => Dialect::Teradata,
        "informix" | "gbase8s" => Dialect::Informix,
        "db2i" => Dialect::Db2i,
        "db2zos" => Dialect::Db2z,
        "sybase" => Dialect::Ase,
        "sqlanywhere" => Dialect::SqlAnywhere,
        "netezza" => Dialect::Netezza,
        "altibase" => Dialect::Altibase,
        "dameng" => Dialect::Dameng,
        "cubrid" => Dialect::Cubrid,
        "zen" => Dialect::Zen,
        "mimer" => Dialect::Mimer,
        "monetdb" => Dialect::MonetDb,
        "ingres" => Dialect::Ingres,
        "iris" | "cache" => Dialect::Iris,
        "maxdb" => Dialect::MaxDb,
        "nuodb" => Dialect::NuoDb,
        "heavydb" => Dialect::HeavyDb,
        "sqream" => Dialect::Sqream,
        "virtuoso" => Dialect::Virtuoso,
        "openedge" => Dialect::OpenEdge,
        "machbase" => Dialect::Machbase,
        "ignite" => Dialect::Ignite,
        "ocient" => Dialect::Ocient,
        _ => return None,
    })
}

/// Why a preset has no users and permissions in DBine.
pub fn unsupported(p: &Preset) -> &'static str {
    match p.id {
        "odbc" => "el motor detrás de un ODBC genérico es desconocido: DBine no sabe cómo lee ni escribe sus usuarios y permisos",
        "spark" | "kyuubi" => "Spark SQL no tiene usuarios ni GRANT propios: la autorización la da el catálogo o Ranger",
        "access" => "Access ya no tiene seguridad por usuarios (se quitó con el formato .accdb) y el driver ODBC no ejecuta sus sentencias de seguridad",
        "dbase" => "los archivos dBase no tienen usuarios ni permisos",
        "ignite3" => "Ignite 3 configura la autenticación en el clúster, no con SQL, y no tiene GRANT",
        "netsuite" => "SuiteAnalytics Connect es de solo lectura: los roles y permisos se administran en NetSuite",
        _ => "DBine no administra usuarios ni permisos de este motor por ODBC",
    }
}

pub fn spec(d: Dialect) -> SecuritySpec {
    match d {
        Dialect::Hive => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALL"],
            object_kinds: vec!["schema", kinds::TABLE, kinds::VIEW],
            create_user: false,
            create_role: true,
            passwords: false,
            membership: true,
            per_database: false,
        },
        Dialect::Impala => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "ALTER", "CREATE", "DROP", "REFRESH", "ALL"],
            // "" = the whole server.
            object_kinds: vec!["", "schema", kinds::TABLE, kinds::VIEW],
            create_user: false,
            create_role: true,
            passwords: false,
            membership: true,
            per_database: false,
        },
        Dialect::Vertica => SecuritySpec {
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "TRUNCATE", "ALTER", "DROP", "USAGE", "CREATE", "ALL"],
            object_kinds: vec!["schema", kinds::TABLE, kinds::VIEW],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
        Dialect::Exasol => SecuritySpec {
            privileges: vec![
                "SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "REFERENCES", "EXECUTE", "CREATE SESSION", "CREATE SCHEMA",
                "CREATE TABLE", "CREATE VIEW", "CREATE USER", "USE ANY CONNECTION", "GRANT ANY PRIVILEGE",
            ],
            // "" = system privileges.
            object_kinds: vec!["", "schema", kinds::TABLE, kinds::VIEW],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        },
        Dialect::Db2 => SecuritySpec {
            privileges: vec![
                "SELECT", "INSERT", "UPDATE", "DELETE", "ALTER", "INDEX", "REFERENCES", "CONTROL", "CONNECT", "CREATETAB",
                "BINDADD", "IMPLICIT_SCHEMA", "LOAD", "DATAACCESS", "ACCESSCTRL", "SQLADM", "DBADM", "SECADM", "CREATEIN",
                "ALTERIN", "DROPIN", "SELECTIN", "INSERTIN", "UPDATEIN", "DELETEIN",
            ],
            // "" = the database.
            object_kinds: vec!["", "schema", kinds::TABLE, kinds::VIEW],
            create_user: false,
            create_role: true,
            passwords: false,
            membership: true,
            per_database: false,
        },
        Dialect::Teradata => teradata::spec(),
        Dialect::Informix => informix::spec(),
        Dialect::Db2i => ibm::spec_i(),
        Dialect::Db2z => ibm::spec_z(),
        Dialect::Ase => sybase::spec_ase(),
        Dialect::SqlAnywhere => sybase::spec_sa(),
        Dialect::Netezza => netezza::spec(),
        Dialect::Altibase | Dialect::Dameng => oracle_like::spec(d),
        Dialect::Cubrid | Dialect::Zen | Dialect::Mimer => groups::spec(d),
        Dialect::MonetDb | Dialect::Ingres | Dialect::Iris | Dialect::MaxDb | Dialect::NuoDb | Dialect::HeavyDb | Dialect::Sqream => sqlstd::spec(d),
        Dialect::Virtuoso | Dialect::OpenEdge | Dialect::Machbase | Dialect::Ignite | Dialect::Ocient => misc::spec(d),
    }
}

// -- names ---------------------------------------------------------------------

const ROLE_PREFIX: &str = "role:";

/// Engines whose statements name the grantee's kind.
fn prefixed(d: Dialect) -> bool {
    matches!(d, Dialect::Hive | Dialect::Impala | Dialect::Db2z | Dialect::Ase | Dialect::Netezza | Dialect::Ingres | Dialect::NuoDb | Dialect::Ocient)
}

fn role_name(d: Dialect, r: &str) -> String {
    if prefixed(d) {
        format!("{ROLE_PREFIX}{r}")
    } else {
        r.to_string()
    }
}

/// A principal's name as `(is a role for sure, name in the engine)`.
pub(crate) fn grantee(name: &str) -> (bool, &str) {
    match name.strip_prefix(ROLE_PREFIX) {
        Some(r) => (true, r),
        None => (false, name),
    }
}

/// SQL reserved words (sorted, upper case): a name spelled like one is
/// always quoted (a schema or user called SELECT or USER). PUBLIC stays
/// out: it's the grantee everyone is.
const RESERVED: &[&str] = &[
    "ABS", "ALL", "ALLOCATE", "ALTER", "AND", "ANY", "ARE", "ARRAY", "AS", "ASENSITIVE", "ASYMMETRIC", "AT", "ATOMIC",
    "AUTHORIZATION", "AVG", "BEGIN", "BETWEEN", "BIGINT", "BINARY", "BLOB", "BOOLEAN", "BOTH", "BY", "CALL", "CALLED",
    "CASCADED", "CASE", "CAST", "CHAR", "CHARACTER", "CHECK", "CLOB", "CLOSE", "COLLATE", "COLUMN", "COMMIT",
    "CONDITION", "CONNECT", "CONSTRAINT", "CONTINUE", "CONVERT", "CORRESPONDING", "COUNT", "CREATE", "CROSS", "CUBE",
    "CURRENT", "CURRENT_CATALOG", "CURRENT_DATE", "CURRENT_PATH", "CURRENT_ROLE", "CURRENT_SCHEMA", "CURRENT_TIME",
    "CURRENT_TIMESTAMP", "CURRENT_USER", "CURSOR", "CYCLE", "DATABASE", "DATE", "DAY", "DEALLOCATE", "DEC", "DECIMAL",
    "DECLARE", "DEFAULT", "DELETE", "DEREF", "DESCRIBE", "DETERMINISTIC", "DISCONNECT", "DISTINCT", "DOUBLE", "DROP",
    "DYNAMIC", "EACH", "ELEMENT", "ELSE", "END", "ESCAPE", "EVERY", "EXCEPT", "EXEC", "EXECUTE", "EXISTS", "EXTERNAL",
    "EXTRACT", "FALSE", "FETCH", "FILTER", "FLOAT", "FOR", "FOREIGN", "FREE", "FROM", "FULL", "FUNCTION", "GET",
    "GLOBAL", "GRANT", "GROUP", "GROUPING", "HAVING", "HOLD", "HOUR", "IDENTITY", "IN", "INDICATOR", "INNER", "INOUT",
    "INSENSITIVE", "INSERT", "INT", "INTEGER", "INTERSECT", "INTERVAL", "INTO", "IS", "JOIN", "LANGUAGE", "LARGE",
    "LATERAL", "LEADING", "LEFT", "LIKE", "LIMIT", "LOCAL", "LOCALTIME", "LOCALTIMESTAMP", "MATCH", "MAX", "MERGE",
    "METHOD", "MIN", "MINUTE", "MODIFIES", "MODULE", "MONTH", "MULTISET", "NATIONAL", "NATURAL", "NCHAR", "NCLOB",
    "NEW", "NO", "NONE", "NOT", "NULL", "NUMERIC", "OF", "OFFSET", "OLD", "ON", "ONLY", "OPEN", "OR", "ORDER", "OUT",
    "OUTER", "OVER", "OVERLAPS", "PARAMETER", "PARTITION", "PRECISION", "PREPARE", "PRIMARY", "PROCEDURE", "RANGE",
    "READS", "REAL", "RECURSIVE", "REF", "REFERENCES", "REFERENCING", "RELEASE", "RESULT", "RETURN", "RETURNS",
    "REVOKE", "RIGHT", "ROLE", "ROLLBACK", "ROLLUP", "ROW", "ROWS", "SAVEPOINT", "SCHEMA", "SCOPE", "SCROLL", "SEARCH",
    "SECOND", "SELECT", "SENSITIVE", "SESSION_USER", "SET", "SIMILAR", "SMALLINT", "SOME", "SPECIFIC", "SQL",
    "SQLEXCEPTION", "SQLSTATE", "SQLWARNING", "START", "STATIC", "SUBMULTISET", "SUM", "SYMMETRIC", "SYSTEM",
    "SYSTEM_USER", "TABLE", "TABLESAMPLE", "THEN", "TIME", "TIMESTAMP", "TIMEZONE_HOUR", "TIMEZONE_MINUTE", "TO",
    "TRAILING", "TRANSLATION", "TREAT", "TRIGGER", "TRUE", "UNION", "UNIQUE", "UNKNOWN", "UNNEST", "UPDATE", "USER",
    "USING", "VALUE", "VALUES", "VARCHAR", "VARYING", "VIEW", "WHEN", "WHENEVER", "WHERE", "WINDOW", "WITH", "WITHIN",
    "WITHOUT", "YEAR",
];

fn reserved(name: &str) -> bool {
    RESERVED.binary_search(&name.to_ascii_uppercase().as_str()).is_ok()
}

/// An identifier: bare when it's a plain one that reads the same after
/// the engine folds it (Db2 and Exasol fold to upper case, Informix to
/// lower case) and isn't a reserved word, quoted otherwise.
pub(crate) fn ident(d: Dialect, name: &str) -> String {
    let folds_up = matches!(
        d,
        Dialect::Db2 | Dialect::Exasol | Dialect::Db2i | Dialect::Db2z | Dialect::Netezza | Dialect::Altibase | Dialect::Dameng | Dialect::Mimer
            | Dialect::MaxDb | Dialect::NuoDb | Dialect::Ignite
    );
    let folds_down = matches!(d, Dialect::Informix | Dialect::MonetDb | Dialect::Ingres | Dialect::Sqream);
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !(folds_up && name.chars().any(|c| c.is_ascii_lowercase()))
        && !(folds_down && name.chars().any(|c| c.is_ascii_uppercase()))
        && !reserved(name);
    if plain {
        return name.to_string();
    }
    match d {
        Dialect::Hive | Dialect::Impala => format!("`{}`", name.replace('`', "``")),
        Dialect::Ase | Dialect::Cubrid => format!("[{}]", name.replace(']', "]]")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// -- reading -------------------------------------------------------------------

/// A row by lower-case column name.
type Row = HashMap<String, String>;

fn get<'a>(r: &'a Row, k: &str) -> &'a str {
    r.get(k).map(String::as_str).unwrap_or("").trim()
}

fn yes(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "t" | "true" | "1" | "y" | "yes")
}

async fn rows(s: &OdbcSession, sql: &str) -> Result<Vec<Row>> {
    let sql = sql.to_string();
    s.run(move |c, slot| {
        let st = c.stmt(slot)?;
        st.exec(&sql)?;
        let n = st.num_cols()?;
        let names: Vec<String> = st.describe(n)?.into_iter().map(|c| c.name.to_ascii_lowercase()).collect();
        let data = st.text_rows()?;
        Ok(data.into_iter().map(|r| names.iter().cloned().zip(r.into_iter().map(Option::unwrap_or_default)).collect()).collect())
    })
    .await
}

/// The first column of each row.
async fn first(s: &OdbcSession, sql: &str) -> Result<Vec<String>> {
    let sql = sql.to_string();
    let data = s
        .run(move |c, slot| {
            let st = c.stmt(slot)?;
            st.exec(&sql)?;
            st.text_rows()
        })
        .await?;
    Ok(data.into_iter().filter_map(|r| r.into_iter().next().flatten()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect())
}

fn user(name: &str) -> Principal {
    Principal { name: name.to_string(), kind: PrincipalKind::User, can_login: Some(true), ..Default::default() }
}

fn role(d: Dialect, name: &str) -> Principal {
    Principal { name: role_name(d, name), kind: PrincipalKind::Role, can_login: Some(false), ..Default::default() }
}

fn add_member(out: &mut Vec<Principal>, member: Principal, of: String) {
    match out.iter_mut().find(|p| p.name == member.name && p.kind == member.kind) {
        Some(p) => {
            if !p.member_of.contains(&of) {
                p.member_of.push(of);
            }
        }
        None => out.push(Principal { member_of: vec![of], ..member }),
    }
}

pub async fn principals(s: &OdbcSession, d: Dialect) -> Result<Vec<Principal>> {
    match d {
        Dialect::Hive => hive_principals(s).await,
        Dialect::Impala => impala_principals(s).await,
        Dialect::Vertica => vertica_principals(s).await,
        Dialect::Exasol => exasol_principals(s).await,
        Dialect::Db2 => db2_principals(s).await,
        Dialect::Teradata => teradata::principals(s).await,
        Dialect::Informix => informix::principals(s).await,
        Dialect::Db2i => ibm::i_principals(s).await,
        Dialect::Db2z => ibm::z_principals(s).await,
        Dialect::Ase => sybase::ase_principals(s).await,
        Dialect::SqlAnywhere => sybase::sa_principals(s).await,
        Dialect::Netezza => netezza::principals(s).await,
        Dialect::Altibase => oracle_like::altibase_principals(s).await,
        Dialect::Dameng => oracle_like::dameng_principals(s).await,
        Dialect::Cubrid => groups::cubrid_principals(s).await,
        Dialect::Zen => groups::zen_principals(s).await,
        Dialect::Mimer => groups::mimer_principals(s).await,
        Dialect::MonetDb => sqlstd::monet_principals(s).await,
        Dialect::Ingres => sqlstd::ingres_principals(s).await,
        Dialect::Iris => sqlstd::iris_principals(s).await,
        Dialect::MaxDb => sqlstd::maxdb_principals(s).await,
        Dialect::NuoDb => sqlstd::nuo_principals(s).await,
        Dialect::HeavyDb => sqlstd::heavy_principals(s).await,
        Dialect::Sqream => sqlstd::sqream_principals(s).await,
        Dialect::Virtuoso => misc::virtuoso_principals(s).await,
        Dialect::OpenEdge => misc::openedge_principals(s).await,
        Dialect::Machbase => misc::machbase_principals(s).await,
        Dialect::Ignite => Ok(misc::ignite_principals()),
        Dialect::Ocient => misc::ocient_principals(s).await,
    }
}

pub async fn grants(s: &OdbcSession, d: Dialect, principal: &str) -> Result<Vec<Grant>> {
    match d {
        Dialect::Hive => hive_grants(s, principal).await,
        Dialect::Impala => impala_grants(s, principal).await,
        Dialect::Vertica => vertica_grants(s, principal).await,
        Dialect::Exasol => exasol_grants(s, principal).await,
        Dialect::Db2 => db2_grants(s, principal).await,
        Dialect::Teradata => teradata::grants(s, principal).await,
        Dialect::Informix => informix::grants(s, principal).await,
        Dialect::Db2i => ibm::i_grants(s, principal).await,
        Dialect::Db2z => ibm::z_grants(s, principal).await,
        Dialect::Ase => sybase::ase_grants(s, principal).await,
        Dialect::SqlAnywhere => sybase::sa_grants(s, principal).await,
        Dialect::Netezza => netezza::grants(s, principal).await,
        Dialect::Altibase => oracle_like::altibase_grants(s, principal).await,
        Dialect::Dameng => oracle_like::dameng_grants(s, principal).await,
        Dialect::Cubrid => groups::cubrid_grants(s, principal).await,
        Dialect::Zen => groups::zen_grants(s, principal).await,
        Dialect::Mimer => groups::mimer_grants(s, principal).await,
        Dialect::MonetDb => sqlstd::monet_grants(s, principal).await,
        Dialect::Ingres => sqlstd::ingres_grants(s, principal).await,
        Dialect::Iris => sqlstd::iris_grants(s, principal).await,
        Dialect::MaxDb => sqlstd::maxdb_grants(s, principal).await,
        Dialect::NuoDb => sqlstd::nuo_grants(s, principal).await,
        Dialect::HeavyDb => sqlstd::heavy_grants(s, principal).await,
        Dialect::Sqream => sqlstd::sqream_grants(s, principal).await,
        Dialect::Virtuoso => misc::virtuoso_grants(s, principal).await,
        Dialect::OpenEdge => misc::openedge_grants(s, principal).await,
        Dialect::Machbase => Err(Error::Unsupported("Machbase no expone en su catálogo los permisos otorgados".into())),
        Dialect::Ignite => Err(Error::Unsupported("Ignite 2 no tiene permisos en SQL: solo usuarios".into())),
        Dialect::Ocient => misc::ocient_grants(s, principal).await,
    }
}

// Hive ------------------------------------------------------------------------

/// Listing roles and principals needs the admin role active.
async fn hive_admin(s: &OdbcSession) {
    let _ = first(s, "SET ROLE ADMIN").await;
}

async fn hive_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    hive_admin(s).await;
    let d = Dialect::Hive;
    let roles = first(s, "SHOW ROLES").await?;
    let mut out: Vec<Principal> = Vec::new();
    if let Some(me) = first(s, "SELECT current_user()").await.ok().and_then(|v| v.into_iter().next()) {
        out.push(Principal { details: vec![("Nota".into(), "Usuario de esta conexión".into())], ..user(&me) });
    }
    for r in &roles {
        let mut p = role(d, r);
        p.system = r.eq_ignore_ascii_case("admin") || r.eq_ignore_ascii_case("public");
        p.superuser = Some(r.eq_ignore_ascii_case("admin"));
        out.push(p);
    }
    for r in &roles {
        for m in rows(s, &format!("SHOW PRINCIPALS {}", ident(d, r))).await.unwrap_or_default() {
            let name = get(&m, "principal_name");
            let member = match get(&m, "principal_type").to_ascii_uppercase().as_str() {
                "USER" => user(name),
                "ROLE" => role(d, name),
                _ => continue,
            };
            add_member(&mut out, member, role_name(d, r));
        }
    }
    Ok(out)
}

/// A Hive or Impala grant row's object: the server, a database or a table.
fn hive_object(r: &Row) -> (Option<String>, Option<String>) {
    let db = get(r, "database");
    let table = get(r, "table");
    let column = get(r, "column");
    if db.is_empty() || db == "*" {
        (None, None)
    } else if table.is_empty() || table == "*" {
        (Some(db.to_string()), Some("schema".into()))
    } else if column.is_empty() || column == "*" {
        (Some(format!("{db}.{table}")), Some(kinds::TABLE.into()))
    } else {
        (Some(format!("{db}.{table}.{column}")), Some("column".into()))
    }
}

fn hive_grant(r: &Row, via: Option<String>) -> Option<Grant> {
    let privilege = get(r, "privilege").to_uppercase();
    if privilege.is_empty() {
        return None;
    }
    let (object, object_kind) = hive_object(r);
    Some(Grant { privilege, object, object_kind, grantable: yes(get(r, "grant_option")), denied: false, via })
}

async fn hive_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    hive_admin(s).await;
    let d = Dialect::Hive;
    let (is_role, name) = grantee(principal);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([(name.to_string(), is_role, None::<String>)]);
    while let Some((n, is_role, via)) = queue.pop_front() {
        if !seen.insert((n.clone(), is_role)) || seen.len() > 64 {
            continue;
        }
        let what = if is_role { "ROLE" } else { "USER" };
        match rows(s, &format!("SHOW GRANT {what} {} ON ALL", ident(d, &n))).await {
            Ok(rs) => out.extend(rs.iter().filter_map(|r| hive_grant(r, via.clone()))),
            Err(e) if via.is_none() => return Err(e),
            Err(_) => {}
        }
        for r in rows(s, &format!("SHOW ROLE GRANT {what} {}", ident(d, &n))).await.unwrap_or_default() {
            let role = get(&r, "role").to_string();
            if !role.is_empty() {
                let v = via.clone().unwrap_or_else(|| role_name(d, &role));
                queue.push_back((role, true, Some(v)));
            }
        }
    }
    Ok(out)
}

// Impala ----------------------------------------------------------------------

async fn impala_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Impala;
    let mut out = Vec::new();
    if let Some(me) = first(s, "SELECT effective_user()").await.ok().and_then(|v| v.into_iter().next()) {
        out.push(Principal { details: vec![("Nota".into(), "Usuario de esta conexión".into())], ..user(&me) });
    }
    for r in first(s, "SHOW ROLES").await? {
        let mut p = role(d, &r);
        p.details.push(("Miembros".into(), "grupos del sistema (GRANT ROLE … TO GROUP …)".into()));
        out.push(p);
    }
    Ok(out)
}

async fn impala_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let d = Dialect::Impala;
    let (is_role, name) = grantee(principal);
    let what = if is_role { "ROLE" } else { "USER" };
    let rs = rows(s, &format!("SHOW GRANT {what} {}", ident(d, name))).await?;
    Ok(rs.iter().filter_map(|r| hive_grant(r, None)).collect())
}

// Vertica ---------------------------------------------------------------------

/// `a*, b` (a `*` marks a default role, or a grantable privilege).
fn list(v: &str) -> Vec<(String, bool)> {
    v.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| (x.trim_end_matches('*').trim().to_string(), x.ends_with('*'))).collect()
}

const VERTICA_SYSTEM_ROLES: &[&str] = &["dbadmin", "pseudosuperuser", "dbduser", "public", "sysmonitor"];

async fn vertica_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Vertica;
    let mut out = Vec::new();
    for u in rows(s, "SELECT * FROM v_catalog.users").await? {
        let name = get(&u, "user_name").to_string();
        let mut details = Vec::new();
        for (k, label) in [("profile_name", "Perfil"), ("resource_pool", "Pool de recursos"), ("lock_time", "Bloqueado desde"), ("default_roles", "Roles predeterminados")] {
            let v = get(&u, k);
            if !v.is_empty() {
                details.push((label.to_string(), v.to_string()));
            }
        }
        out.push(Principal {
            superuser: Some(yes(get(&u, "is_super_user"))),
            disabled: Some(yes(get(&u, "is_locked"))),
            member_of: list(get(&u, "all_roles")).into_iter().map(|(r, _)| r).collect(),
            system: yes(get(&u, "is_super_user")) && name.eq_ignore_ascii_case("dbadmin"),
            details,
            ..user(&name)
        });
    }
    for r in rows(s, "SELECT * FROM v_catalog.roles").await.unwrap_or_default() {
        let name = get(&r, "name").to_string();
        out.push(Principal {
            member_of: list(get(&r, "assigned_roles")).into_iter().map(|(r, _)| r).collect(),
            system: VERTICA_SYSTEM_ROLES.contains(&name.to_ascii_lowercase().as_str()),
            superuser: Some(name.eq_ignore_ascii_case("dbadmin") || name.eq_ignore_ascii_case("pseudosuperuser")),
            ..role(d, &name)
        });
    }
    Ok(out)
}

fn vertica_kind(t: &str) -> Option<&'static str> {
    Some(match t.to_ascii_uppercase().as_str() {
        "TABLE" => kinds::TABLE,
        "VIEW" => kinds::VIEW,
        "SCHEMA" => "schema",
        "DATABASE" => "database",
        "PROCEDURE" => kinds::PROCEDURE,
        "FUNCTION" => kinds::FUNCTION,
        "SEQUENCE" => "sequence",
        "RESOURCEPOOL" => "resourcepool",
        "LIBRARY" => "library",
        "MODEL" => "model",
        _ => return None,
    })
}

fn vertica_grants_of(all: &[Row], name: &str, via: Option<String>) -> Vec<Grant> {
    let mut out = Vec::new();
    for r in all.iter().filter(|r| get(r, "grantee").eq_ignore_ascii_case(name)) {
        let ty = get(r, "object_type");
        // Role grants are memberships, listed with the principals.
        let Some(kind) = vertica_kind(ty) else { continue };
        let schema = get(r, "object_schema");
        let obj = get(r, "object_name");
        let object = match kind {
            "schema" | "database" | "resourcepool" => obj.to_string(),
            _ if schema.is_empty() => obj.to_string(),
            _ => format!("{schema}.{obj}"),
        };
        for (p, grantable) in list(get(r, "privileges_description")) {
            out.push(Grant { privilege: p.to_uppercase(), object: Some(object.clone()), object_kind: Some(kind.into()), grantable, denied: false, via: via.clone() });
        }
    }
    out
}

/// Follows role memberships from `members` (principal → its roles).
fn with_roles(name: &str, members: &HashMap<String, Vec<String>>, mut grants_of: impl FnMut(&str, Option<String>) -> Vec<Grant>) -> Vec<Grant> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([(name.to_string(), None::<String>)]);
    while let Some((n, via)) = queue.pop_front() {
        if !seen.insert(n.to_ascii_lowercase()) || seen.len() > 64 {
            continue;
        }
        out.extend(grants_of(&n, via.clone()));
        for r in members.get(&n.to_ascii_lowercase()).into_iter().flatten() {
            queue.push_back((r.clone(), Some(via.clone().unwrap_or_else(|| r.clone()))));
        }
    }
    out
}

async fn vertica_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let all = rows(s, "SELECT grantee, privileges_description, object_schema, object_name, object_type FROM v_catalog.grants").await?;
    let members: HashMap<String, Vec<String>> =
        vertica_principals(s).await?.into_iter().map(|p| (p.name.to_ascii_lowercase(), p.member_of)).collect();
    Ok(with_roles(principal, &members, |n, via| vertica_grants_of(&all, n, via)))
}

// Exasol ----------------------------------------------------------------------

async fn exasol_role_privs(s: &OdbcSession) -> HashMap<String, Vec<String>> {
    let mut m: HashMap<String, Vec<String>> = HashMap::new();
    for r in rows(s, "SELECT GRANTEE, GRANTED_ROLE FROM EXA_DBA_ROLE_PRIVS").await.unwrap_or_default() {
        m.entry(get(&r, "grantee").to_ascii_lowercase()).or_default().push(get(&r, "granted_role").to_string());
    }
    m
}

async fn exasol_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Exasol;
    let users = match rows(s, "SELECT * FROM EXA_DBA_USERS").await {
        Ok(u) => u,
        // Without the DBA views: the users this one can see.
        Err(_) => rows(s, "SELECT * FROM EXA_ALL_USERS").await?,
    };
    let members = exasol_role_privs(s).await;
    let mut out = Vec::new();
    for u in &users {
        let name = get(u, "user_name").to_string();
        let member_of = members.get(&name.to_ascii_lowercase()).cloned().unwrap_or_default();
        let mut details = Vec::new();
        for (k, label) in [
            ("created", "Alta"),
            ("password_state", "Contraseña"),
            ("password_expiry", "Vence"),
            ("distinguished_name", "LDAP"),
            ("kerberos_principal", "Kerberos"),
            ("user_consumer_group", "Grupo de consumo"),
        ] {
            let v = get(u, k);
            if !v.is_empty() {
                details.push((label.to_string(), v.to_string()));
            }
        }
        out.push(Principal {
            superuser: Some(name == "SYS" || member_of.iter().any(|r| r == "DBA")),
            system: name == "SYS",
            member_of,
            details,
            ..user(&name)
        });
    }
    for r in rows(s, "SELECT ROLE_NAME FROM EXA_DBA_ROLES").await.unwrap_or_default() {
        let name = get(&r, "role_name").to_string();
        out.push(Principal {
            member_of: members.get(&name.to_ascii_lowercase()).cloned().unwrap_or_default(),
            system: name == "DBA" || name == "PUBLIC",
            superuser: Some(name == "DBA"),
            ..role(d, &name)
        });
    }
    Ok(out)
}

fn exasol_kind(t: &str) -> &'static str {
    match t.to_ascii_uppercase().as_str() {
        "VIEW" => kinds::VIEW,
        "SCHEMA" => "schema",
        "FUNCTION" => kinds::FUNCTION,
        "SCRIPT" => "script",
        "CONNECTION" => "connection",
        _ => kinds::TABLE,
    }
}

async fn exasol_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let sys = rows(s, "SELECT GRANTEE, PRIVILEGE, ADMIN_OPTION FROM EXA_DBA_SYS_PRIVS").await?;
    let obj = rows(s, "SELECT GRANTEE, PRIVILEGE, OBJECT_SCHEMA, OBJECT_NAME, OBJECT_TYPE FROM EXA_DBA_OBJ_PRIVS").await?;
    let members = exasol_role_privs(s).await;
    Ok(with_roles(principal, &members, |n, via| {
        let mut out: Vec<Grant> = sys
            .iter()
            .filter(|r| get(r, "grantee") == n)
            .map(|r| Grant { privilege: get(r, "privilege").to_string(), grantable: yes(get(r, "admin_option")), via: via.clone(), ..Default::default() })
            .collect();
        for r in obj.iter().filter(|r| get(r, "grantee") == n) {
            let kind = exasol_kind(get(r, "object_type"));
            let (schema, name) = (get(r, "object_schema"), get(r, "object_name"));
            let object = if kind == "schema" || schema.is_empty() { name.to_string() } else { format!("{schema}.{name}") };
            out.push(Grant { privilege: get(r, "privilege").to_string(), object: Some(object), object_kind: Some(kind.into()), via: via.clone(), ..Default::default() });
        }
        out
    }))
}

// Db2 -------------------------------------------------------------------------

/// Privilege names of the catalog's `…AUTH` columns that aren't the
/// column's name without `AUTH`.
fn db2_privilege(col: &str) -> Option<String> {
    let base = col.strip_suffix("auth")?.to_ascii_uppercase();
    Some(
        match base.as_str() {
            "REF" | "REFERENCE" => "REFERENCES",
            "IMPLSCHEMA" => "IMPLICIT_SCHEMA",
            "SECURITYADM" => "SECADM",
            "NOFENCE" => "CREATE_NOT_FENCED_ROUTINE",
            "EXTERNALROUTINE" => "CREATE_EXTERNAL_ROUTINE",
            "QUIESCECONNECT" => "QUIESCE_CONNECT",
            "CREATESECURE" => "CREATE_SECURE_OBJECT",
            b => b,
        }
        .to_string(),
    )
}

/// Every `…AUTH` column of a catalog row that's `Y` (held) or `G`
/// (grantable).
fn db2_auths(r: &Row, object: Option<String>, kind: Option<&str>, via: &Option<String>) -> Vec<Grant> {
    let mut cols: Vec<(&String, &String)> = r.iter().filter(|(k, _)| k.ends_with("auth")).collect();
    cols.sort();
    cols.into_iter()
        .filter_map(|(k, v)| {
            let v = v.trim().to_ascii_uppercase();
            (v == "Y" || v == "G").then(|| Grant {
                privilege: db2_privilege(k).unwrap_or_default(),
                object: object.clone(),
                object_kind: kind.map(str::to_string),
                grantable: v == "G",
                denied: false,
                via: via.clone(),
            })
        })
        .collect()
}

async fn db2_principals(s: &OdbcSession) -> Result<Vec<Principal>> {
    let d = Dialect::Db2;
    let db = rows(s, "SELECT GRANTEE, GRANTEETYPE, DBADMAUTH, CONNECTAUTH, SECURITYADMAUTH FROM SYSCAT.DBAUTH").await?;
    let others = first(
        s,
        "SELECT DISTINCT GRANTEE FROM SYSCAT.TABAUTH WHERE GRANTEETYPE = 'U'
          UNION SELECT DISTINCT GRANTEE FROM SYSCAT.SCHEMAAUTH WHERE GRANTEETYPE = 'U'
          UNION SELECT DISTINCT GRANTEE FROM SYSCAT.ROLEAUTH WHERE GRANTEETYPE = 'U'",
    )
    .await
    .unwrap_or_default();
    let members = rows(s, "SELECT GRANTEE, GRANTEETYPE, ROLENAME FROM SYSCAT.ROLEAUTH").await.unwrap_or_default();
    let member_of = |name: &str, ty: &str| -> Vec<String> {
        members.iter().filter(|m| get(m, "grantee") == name && get(m, "granteetype") == ty).map(|m| get(m, "rolename").to_string()).collect()
    };
    let mut out: Vec<Principal> = Vec::new();
    for r in db.iter().filter(|r| get(r, "granteetype") == "U") {
        let name = get(r, "grantee");
        out.push(Principal {
            superuser: Some(get(r, "dbadmauth") == "Y" || get(r, "securityadmauth") == "Y"),
            can_login: Some(get(r, "connectauth") == "Y"),
            member_of: member_of(name, "U"),
            details: vec![("Autenticación".into(), "del sistema operativo o LDAP".into())],
            ..user(name)
        });
    }
    for name in others {
        if !out.iter().any(|p| p.name == name) {
            out.push(Principal {
                can_login: None,
                member_of: member_of(&name, "U"),
                details: vec![("Autenticación".into(), "del sistema operativo o LDAP".into())],
                ..user(&name)
            });
        }
    }
    for r in rows(s, "SELECT ROLENAME FROM SYSCAT.ROLES").await.unwrap_or_default() {
        let name = get(&r, "rolename").to_string();
        out.push(Principal { member_of: member_of(&name, "R"), system: name.starts_with("SYS"), ..role(d, &name) });
    }
    Ok(out)
}

async fn db2_grants(s: &OdbcSession, principal: &str) -> Result<Vec<Grant>> {
    let roles: HashSet<String> = first(s, "SELECT ROLENAME FROM SYSCAT.ROLES").await.unwrap_or_default().into_iter().collect();
    let members = rows(s, "SELECT GRANTEE, GRANTEETYPE, ROLENAME FROM SYSCAT.ROLEAUTH").await.unwrap_or_default();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let first_ty = if roles.contains(principal) { "R" } else { "U" };
    let mut queue = VecDeque::from([(principal.to_string(), first_ty, None::<String>)]);
    while let Some((n, ty, via)) = queue.pop_front() {
        if !seen.insert(n.clone()) || seen.len() > 64 {
            continue;
        }
        let who = format!("GRANTEE = {} AND GRANTEETYPE = '{ty}'", lit(&n));
        for r in rows(s, &format!("SELECT * FROM SYSCAT.DBAUTH WHERE {who}")).await? {
            out.extend(db2_auths(&r, None, None, &via));
        }
        for r in rows(s, &format!("SELECT * FROM SYSCAT.SCHEMAAUTH WHERE {who}")).await.unwrap_or_default() {
            out.extend(db2_auths(&r, Some(get(&r, "schemaname").to_string()), Some("schema"), &via));
        }
        let tables = format!(
            "SELECT a.*, t.TYPE AS OBJTYPE FROM SYSCAT.TABAUTH a JOIN SYSCAT.TABLES t ON t.TABSCHEMA = a.TABSCHEMA AND t.TABNAME = a.TABNAME WHERE a.{who}"
        );
        for r in rows(s, &tables).await.unwrap_or_default() {
            let kind = if get(&r, "objtype") == "V" { kinds::VIEW } else { kinds::TABLE };
            out.extend(db2_auths(&r, Some(format!("{}.{}", get(&r, "tabschema"), get(&r, "tabname"))), Some(kind), &via));
        }
        for m in members.iter().filter(|m| get(m, "grantee") == n && get(m, "granteetype") == ty) {
            let role = get(m, "rolename").to_string();
            let v = via.clone().unwrap_or_else(|| role.clone());
            queue.push_back((role, "R", Some(v)));
        }
    }
    Ok(out)
}

// -- scripts -------------------------------------------------------------------

fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let up = x.trim().to_uppercase();
        // `%` starts IRIS's administrative privileges (%CREATE_TABLE);
        // z/OS has MONITOR1 and MONITOR2.
        let ok = up.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '%')
            && up.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '%');
        if !ok {
            return Err(Error::Query(format!("«{x}» no es un permiso válido")));
        }
        out.push(up);
    }
    Ok(out.join(", "))
}

/// `GRANT`/`REVOKE` statements for these privileges: `ALL` goes alone
/// (`GRANT ALL, SELECT …` is a syntax error), and Impala takes a single
/// privilege per statement, so it gets one statement each.
fn per_privilege(d: Dialect, p: &[String], stmt: impl Fn(String) -> String) -> Result<String> {
    let all = p.iter().any(|x| matches!(x.trim().to_uppercase().as_str(), "ALL" | "ALL PRIVILEGES"));
    if all && p.len() > 1 {
        return Err(Error::Query("ALL ya incluye todos los permisos: elegilo solo, sin otros".into()));
    }
    if matches!(d, Dialect::Impala) {
        let each: Result<Vec<String>> = p.iter().map(|x| privileges(std::slice::from_ref(x)).map(&stmt)).collect();
        return Ok(each?.join("\n"));
    }
    Ok(stmt(privileges(p)?))
}

/// `schema.name`, each part as an identifier.
fn qualified(d: Dialect, o: &ObjectRef) -> String {
    match o.schema().filter(|s| !s.is_empty()) {
        Some(sc) => format!("{}.{}", ident(d, sc), ident(d, &o.name)),
        None => ident(d, &o.name),
    }
}

/// ` ON …` (empty for Exasol's system privileges and Db2's database ones,
/// which have their own form).
fn on(d: Dialect, object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else {
        return match d {
            Dialect::Impala => Ok(" ON SERVER".into()),
            Dialect::Db2 => Ok(" ON DATABASE".into()),
            Dialect::Exasol => Ok(String::new()),
            _ => {
                Err(Error::Query("elegí una base de datos, un esquema o una tabla: no hay permisos sobre todo el servidor en este motor".into()))
            }
        };
    };
    let schema = o.kind == "schema" || o.kind == "database";
    Ok(match d {
        Dialect::Hive | Dialect::Impala if schema => format!(" ON DATABASE {}", ident(d, &o.name)),
        Dialect::Hive | Dialect::Impala => format!(" ON TABLE {}", qualified(d, o)),
        Dialect::Vertica | Dialect::Exasol | Dialect::Db2 if schema => format!(" ON SCHEMA {}", ident(d, &o.name)),
        Dialect::Exasol if o.kind == kinds::VIEW => format!(" ON VIEW {}", qualified(d, o)),
        Dialect::Exasol if o.kind == kinds::FUNCTION => format!(" ON FUNCTION {}", qualified(d, o)),
        Dialect::Exasol if o.kind == "script" => format!(" ON SCRIPT {}", qualified(d, o)),
        Dialect::Vertica if o.kind == kinds::VIEW => format!(" ON {}", qualified(d, o)),
        _ => format!(" ON TABLE {}", qualified(d, o)),
    })
}

fn to_whom(d: Dialect, name: &str) -> String {
    let (is_role, n) = grantee(name);
    match d {
        Dialect::Hive | Dialect::Impala if is_role => format!("ROLE {}", ident(d, n)),
        Dialect::Hive | Dialect::Impala => format!("USER {}", ident(d, n)),
        _ => ident(d, n),
    }
}

/// A new user's password, required.
fn password(p: &Option<String>) -> Result<&str> {
    p.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))
}

fn option(grantable: bool) -> &'static str {
    if grantable {
        " WITH GRANT OPTION"
    } else {
        ""
    }
}

fn unsupported_action<T>(why: &str) -> Result<T> {
    Err(Error::Unsupported(why.into()))
}

const EXTERNAL_USERS: &str = "los usuarios de este motor se autentican afuera (sistema operativo, LDAP o Kerberos): no se crean, borran ni cambian desde SQL";

pub fn script(d: Dialect, a: &SecurityAction) -> Result<String> {
    match d {
        Dialect::Teradata => return teradata::script(a),
        Dialect::Informix => return informix::script(a),
        Dialect::Db2i | Dialect::Db2z => return ibm::script(d, a),
        Dialect::Ase => return sybase::ase_script(a),
        Dialect::SqlAnywhere => return sybase::sa_script(a),
        Dialect::Netezza => return netezza::script(a),
        Dialect::Altibase | Dialect::Dameng => return oracle_like::script(d, a),
        Dialect::Cubrid | Dialect::Zen | Dialect::Mimer => return groups::script(d, a),
        Dialect::MonetDb | Dialect::Ingres | Dialect::Iris | Dialect::MaxDb | Dialect::NuoDb | Dialect::HeavyDb | Dialect::Sqream => {
            return sqlstd::script(d, a)
        }
        Dialect::Virtuoso | Dialect::OpenEdge | Dialect::Machbase | Dialect::Ignite | Dialect::Ocient => return misc::script(d, a),
        Dialect::Hive | Dialect::Impala | Dialect::Vertica | Dialect::Exasol | Dialect::Db2 => {}
    }
    let external = matches!(d, Dialect::Hive | Dialect::Impala | Dialect::Db2);
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            if external {
                return Err(Error::Unsupported(EXTERNAL_USERS.into()));
            }
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            match d {
                // Exasol's password is an identifier.
                Dialect::Exasol => format!("CREATE USER {} IDENTIFIED BY \"{}\";", ident(d, name), pw.replace('"', "\"\"")),
                _ => format!("CREATE USER {} IDENTIFIED BY {};", ident(d, name), lit(pw)),
            }
        }
        SecurityAction::SetPassword { name, password } => match d {
            _ if external => return Err(Error::Unsupported(EXTERNAL_USERS.into())),
            Dialect::Exasol => format!("ALTER USER {} IDENTIFIED BY \"{}\";", ident(d, name), password.replace('"', "\"\"")),
            _ => format!("ALTER USER {} IDENTIFIED BY {};", ident(d, name), lit(password)),
        },
        SecurityAction::SetLogin { name, enabled } => match d {
            Dialect::Vertica => format!("ALTER USER {} ACCOUNT {};", ident(d, name), if *enabled { "UNLOCK" } else { "LOCK" }),
            Dialect::Db2 => format!("{} CONNECT ON DATABASE {} {};", if *enabled { "GRANT" } else { "REVOKE" }, if *enabled { "TO" } else { "FROM" }, ident(d, name)),
            Dialect::Exasol => return Err(Error::Unsupported("Exasol no deshabilita usuarios: quitale CREATE SESSION o cambiale la contraseña".into())),
            _ => return Err(Error::Unsupported(EXTERNAL_USERS.into())),
        },
        SecurityAction::Drop { name, kind: PrincipalKind::User } => {
            if external {
                return Err(Error::Unsupported(EXTERNAL_USERS.into()));
            }
            format!("DROP USER {};", ident(d, name))
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", ident(d, grantee(name).1)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", ident(d, grantee(name).1)),
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            let option = match (d, grantable, object) {
                (_, false, _) => "",
                (Dialect::Exasol, true, None) => " WITH ADMIN OPTION",
                (Dialect::Exasol, true, Some(_)) => return Err(Error::Unsupported("Exasol no otorga permisos sobre objetos con opción de otorgarlos a otros".into())),
                (Dialect::Db2, true, None) => return Err(Error::Unsupported("Db2 no otorga permisos de la base con opción de otorgarlos a otros".into())),
                _ => " WITH GRANT OPTION",
            };
            let (on, to) = (on(d, object)?, to_whom(d, to));
            per_privilege(d, p, |x| format!("GRANT {x}{on} TO {to}{option};"))?
        }
        SecurityAction::Revoke { privileges: p, object, from } => {
            let (on, from) = (on(d, object)?, to_whom(d, from));
            per_privilege(d, p, |x| format!("REVOKE {x}{on} FROM {from};"))?
        }
        SecurityAction::AddMember { role, member } => {
            let r = ident(d, grantee(role).1);
            match d {
                // Impala's roles go to the system's groups.
                Dialect::Impala => format!("GRANT ROLE {r} TO GROUP {};", ident(d, grantee(member).1)),
                Dialect::Hive => format!("GRANT ROLE {r} TO {};", to_whom(d, member)),
                Dialect::Db2 => format!("GRANT ROLE {r} TO {};", ident(d, member)),
                _ => format!("GRANT {r} TO {};", ident(d, member)),
            }
        }
        SecurityAction::RemoveMember { role, member } => {
            let r = ident(d, grantee(role).1);
            match d {
                Dialect::Impala => format!("REVOKE ROLE {r} FROM GROUP {};", ident(d, grantee(member).1)),
                Dialect::Hive => format!("REVOKE ROLE {r} FROM {};", to_whom(d, member)),
                Dialect::Db2 => format!("REVOKE ROLE {r} FROM {};", ident(d, member)),
                _ => format!("REVOKE {r} FROM {};", ident(d, member)),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    fn grant(p: &str, object: Option<ObjectRef>, to: &str, grantable: bool) -> SecurityAction {
        SecurityAction::Grant { privileges: vec![p.into()], object, to: to.into(), grantable }
    }

    #[test]
    fn only_the_known_presets() {
        let ids: Vec<&str> = crate::PRESETS.iter().filter(|p| dialect(p).is_some()).map(|p| p.id).collect();
        assert_eq!(
            ids,
            vec![
                "db2", "db2i", "db2zos", "sybase", "sqlanywhere", "hive", "impala", "informix", "teradata", "vertica", "exasol", "netezza",
                "altibase", "cubrid", "dameng", "gbase8s", "ocient", "cloudera", "monetdb", "virtuoso", "ingres", "mimer", "iris", "cache",
                "openedge", "zen", "sqream", "maxdb", "nuodb", "heavydb", "machbase", "ignite"
            ]
        );
    }

    #[test]
    fn hive_and_impala() {
        let (h, i) = (Dialect::Hive, Dialect::Impala);
        let s = |d, a| script(d, &a).unwrap();
        assert_eq!(s(h, SecurityAction::CreateRole { name: "lect".into() }), "CREATE ROLE lect;");
        assert_eq!(s(h, SecurityAction::Drop { name: "role:lect".into(), kind: PrincipalKind::Role }), "DROP ROLE lect;");
        assert_eq!(s(h, grant("select", obj(kinds::TABLE, Some("ventas"), "fac`t"), "role:lect", true)), "GRANT SELECT ON TABLE ventas.`fac``t` TO ROLE lect WITH GRANT OPTION;");
        assert_eq!(s(h, grant("INSERT", obj("schema", None, "ventas"), "ana", false)), "GRANT INSERT ON DATABASE ventas TO USER ana;");
        assert!(script(h, &grant("SELECT", None, "ana", false)).is_err());
        assert_eq!(s(h, SecurityAction::AddMember { role: "role:lect".into(), member: "ana".into() }), "GRANT ROLE lect TO USER ana;");
        assert_eq!(s(h, SecurityAction::RemoveMember { role: "role:lect".into(), member: "role:otro".into() }), "REVOKE ROLE lect FROM ROLE otro;");
        assert_eq!(s(i, grant("ALL", None, "role:lect", false)), "GRANT ALL ON SERVER TO ROLE lect;");
        assert_eq!(s(i, SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: obj(kinds::VIEW, Some("db"), "v"), from: "role:lect".into() }), "REVOKE SELECT ON TABLE db.v FROM ROLE lect;");
        assert_eq!(s(i, SecurityAction::AddMember { role: "role:lect".into(), member: "analistas".into() }), "GRANT ROLE lect TO GROUP analistas;");
        for d in [h, i] {
            for a in [
                SecurityAction::CreateUser { name: "a".into(), password: Some("p".into()) },
                SecurityAction::SetPassword { name: "a".into(), password: "p".into() },
                SecurityAction::Drop { name: "a".into(), kind: PrincipalKind::User },
                SecurityAction::SetLogin { name: "a".into(), enabled: false },
            ] {
                assert!(matches!(script(d, &a), Err(Error::Unsupported(_))));
            }
        }
    }

    #[test]
    fn reserved_words_quoted() {
        assert!(RESERVED.windows(2).all(|w| w[0] < w[1]), "RESERVED must stay sorted");
        assert_eq!(ident(Dialect::Db2, "USER"), "\"USER\"");
        assert_eq!(ident(Dialect::Vertica, "select"), "\"select\"");
        assert_eq!(ident(Dialect::Hive, "table"), "`table`");
        assert_eq!(ident(Dialect::Db2, "PUBLIC"), "PUBLIC");
        assert_eq!(ident(Dialect::Db2, "USERS"), "USERS");
    }

    #[test]
    fn vertica_and_exasol() {
        let (v, e) = (Dialect::Vertica, Dialect::Exasol);
        let s = |d, a| script(d, &a).unwrap();
        assert_eq!(s(v, SecurityAction::CreateUser { name: "ana b".into(), password: Some("p'w".into()) }), "CREATE USER \"ana b\" IDENTIFIED BY 'p''w';");
        assert_eq!(s(v, SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ALTER USER ana ACCOUNT LOCK;");
        assert_eq!(s(v, grant("SELECT", obj(kinds::VIEW, Some("public"), "v"), "lect", false)), "GRANT SELECT ON public.v TO lect;");
        assert_eq!(s(v, grant("USAGE", obj("schema", None, "ventas"), "ana", true)), "GRANT USAGE ON SCHEMA ventas TO ana WITH GRANT OPTION;");
        assert_eq!(s(v, SecurityAction::AddMember { role: "lect".into(), member: "ana".into() }), "GRANT lect TO ana;");
        assert_eq!(s(v, SecurityAction::RemoveMember { role: "lect".into(), member: "ana".into() }), "REVOKE lect FROM ana;");
        assert_eq!(s(e, SecurityAction::CreateUser { name: "ana".into(), password: Some("p\"w".into()) }), "CREATE USER \"ana\" IDENTIFIED BY \"p\"\"w\";");
        assert_eq!(s(e, SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }), "ALTER USER \"ana\" IDENTIFIED BY \"x\";");
        assert_eq!(s(e, grant("CREATE SESSION", None, "ana", true)), "GRANT CREATE SESSION TO \"ana\" WITH ADMIN OPTION;");
        assert_eq!(s(e, grant("SELECT", obj(kinds::VIEW, Some("S"), "V"), "LECT", false)), "GRANT SELECT ON VIEW S.V TO LECT;");
        assert_eq!(s(e, SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: obj("schema", None, "S"), from: "LECT".into() }), "REVOKE SELECT ON SCHEMA S FROM LECT;");
        assert!(matches!(script(e, &grant("SELECT", obj(kinds::TABLE, Some("S"), "T"), "a", true)), Err(Error::Unsupported(_))));
        assert!(script(v, &grant("SELECT; DROP TABLE x", None, "a", false)).is_err());
    }

    #[test]
    fn db2() {
        let d = Dialect::Db2;
        let s = |a| script(d, &a).unwrap();
        assert_eq!(s(grant("CONNECT", None, "ANA", false)), "GRANT CONNECT ON DATABASE TO ANA;");
        assert_eq!(s(grant("SELECT", obj(kinds::TABLE, Some("DB2INST1"), "Fact"), "LECT", true)), "GRANT SELECT ON TABLE DB2INST1.\"Fact\" TO LECT WITH GRANT OPTION;");
        assert_eq!(s(grant("CREATEIN", obj("schema", None, "VENTAS"), "ANA", false)), "GRANT CREATEIN ON SCHEMA VENTAS TO ANA;");
        assert_eq!(s(SecurityAction::SetLogin { name: "ANA".into(), enabled: false }), "REVOKE CONNECT ON DATABASE FROM ANA;");
        assert_eq!(s(SecurityAction::AddMember { role: "LECT".into(), member: "ANA".into() }), "GRANT ROLE LECT TO ANA;");
        assert_eq!(s(SecurityAction::CreateRole { name: "lect".into() }), "CREATE ROLE \"lect\";");
        assert!(matches!(script(d, &grant("CONNECT", None, "A", true)), Err(Error::Unsupported(_))));
        assert!(matches!(script(d, &SecurityAction::CreateUser { name: "a".into(), password: Some("p".into()) }), Err(Error::Unsupported(_))));
    }

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn reads_catalog_rows() {
        let g = hive_grant(&row(&[("database", "ventas"), ("table", "f"), ("privilege", "select"), ("grant_option", "true")]), Some("role:lect".into())).unwrap();
        assert_eq!((g.privilege.as_str(), g.object.as_deref(), g.object_kind.as_deref(), g.grantable), ("SELECT", Some("ventas.f"), Some(kinds::TABLE), true));
        assert_eq!(hive_grant(&row(&[("database", "*"), ("privilege", "ALL")]), None).unwrap().object, None);
        assert_eq!(list("pseudosuperuser*, dbduser*, lect"), vec![("pseudosuperuser".into(), true), ("dbduser".into(), true), ("lect".into(), false)]);
        let all = vec![
            row(&[("grantee", "ana"), ("privileges_description", "SELECT*, INSERT"), ("object_schema", "public"), ("object_name", "t"), ("object_type", "TABLE")]),
            row(&[("grantee", "ana"), ("privileges_description", ""), ("object_schema", ""), ("object_name", "lect"), ("object_type", "ROLE")]),
            row(&[("grantee", "lect"), ("privileges_description", "USAGE"), ("object_schema", ""), ("object_name", "ventas"), ("object_type", "SCHEMA")]),
        ];
        let members = HashMap::from([("ana".to_string(), vec!["lect".to_string()])]);
        let g = with_roles("ana", &members, |n, via| vertica_grants_of(&all, n, via));
        assert_eq!(g.len(), 3);
        assert!(g.iter().any(|x| x.privilege == "SELECT" && x.grantable && x.object.as_deref() == Some("public.t")));
        assert!(g.iter().any(|x| x.privilege == "USAGE" && x.via.as_deref() == Some("lect") && x.object_kind.as_deref() == Some("schema")));
        let d = db2_auths(&row(&[("grantee", "ANA"), ("selectauth", "G"), ("insertauth", "Y"), ("deleteauth", "N"), ("refauth", "Y")]), Some("S.T".into()), Some(kinds::TABLE), &None);
        let names: Vec<(&str, bool)> = d.iter().map(|g| (g.privilege.as_str(), g.grantable)).collect();
        assert_eq!(names, vec![("INSERT", false), ("REFERENCES", false), ("SELECT", true)]);
        assert_eq!(db2_privilege("implschemaauth").as_deref(), Some("IMPLICIT_SCHEMA"));
    }
}

#[cfg(test)]
mod more_engines {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    fn grant(p: &str, object: Option<ObjectRef>, to: &str, grantable: bool) -> SecurityAction {
        SecurityAction::Grant { privileges: vec![p.into()], object, to: to.into(), grantable }
    }

    fn user_pw(name: &str, pw: &str) -> SecurityAction {
        SecurityAction::CreateUser { name: name.into(), password: Some(pw.into()) }
    }

    fn member(role: &str, member: &str) -> SecurityAction {
        SecurityAction::AddMember { role: role.into(), member: member.into() }
    }

    fn refused(d: Dialect, a: SecurityAction) -> bool {
        matches!(script(d, &a), Err(Error::Unsupported(_)))
    }

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn every_preset_has_security_or_a_reason() {
        let none: Vec<&str> = crate::PRESETS.iter().filter(|p| dialect(p).is_none()).map(|p| p.id).collect();
        assert_eq!(none, vec!["odbc", "spark", "kyuubi", "access", "dbase", "ignite3", "netsuite"]);
        for p in crate::PRESETS.iter().filter(|p| dialect(p).is_none()) {
            assert_ne!(super::unsupported(p), super::unsupported(&crate::PRESETS[1]), "{}", p.id);
        }
        for p in crate::PRESETS.iter() {
            if let Some(d) = dialect(p) {
                let s = spec(d);
                assert!(s.privileges.iter().all(|x| privileges(&[x.to_string()]).is_ok()), "{}", p.id);
            }
        }
    }

    #[test]
    fn teradata() {
        let d = Dialect::Teradata;
        let s = |a| script(d, &a).unwrap();
        assert_eq!(s(user_pw("ana", "p\"w")), "CREATE USER ana AS PERMANENT = 0, PASSWORD = \"p\"\"w\";");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }), "MODIFY USER ana AS PASSWORD = \"x\";");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "REVOKE LOGON ON ALL FROM ana;");
        assert!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: true }).contains("RELEASE PASSWORD LOCK"));
        assert_eq!(s(grant("SELECT", obj(kinds::TABLE, Some("ventas"), "fact"), "lect", true)), "GRANT SELECT ON ventas.fact TO lect WITH GRANT OPTION;");
        assert_eq!(s(grant("CREATE TABLE", obj("schema", None, "ventas"), "ana", false)), "GRANT CREATE TABLE ON ventas TO ana;");
        assert_eq!(s(grant("EXECUTE PROCEDURE", obj(kinds::PROCEDURE, Some("v"), "p"), "ana", false)), "GRANT EXECUTE PROCEDURE ON PROCEDURE v.p TO ana;");
        assert!(script(d, &grant("SELECT", None, "ana", false)).is_err());
        assert_eq!(s(member("lect", "ana")), "GRANT lect TO ana;");
        assert_eq!(teradata::right("R"), "SELECT");
        assert_eq!(teradata::right("PE"), "EXECUTE PROCEDURE");
        assert_eq!(teradata::right("ZZ"), "ZZ");
        let g = teradata::grant_of(&row(&[("databasename", "ventas"), ("tablename", "All"), ("accessright", "CT"), ("grantauthority", "Y")]), None).unwrap();
        assert_eq!((g.privilege.as_str(), g.object.as_deref(), g.object_kind.as_deref(), g.grantable), ("CREATE TABLE", Some("ventas"), Some("schema"), true));
        let g = teradata::grant_of(&row(&[("databasename", "ventas"), ("tablename", "v1"), ("accessright", "R"), ("tablekind", "V")]), Some("lect".into())).unwrap();
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref(), g.via.as_deref()), (Some("ventas.v1"), Some(kinds::VIEW), Some("lect")));
    }

    #[test]
    fn informix() {
        let d = Dialect::Informix;
        let s = |a| script(d, &a).unwrap();
        assert!(refused(d, user_pw("ana", "x")));
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "REVOKE CONNECT FROM ana;");
        assert_eq!(s(grant("RESOURCE", None, "ana", false)), "GRANT RESOURCE TO ana;");
        assert!(refused(d, grant("DBA", None, "ana", true)));
        assert_eq!(s(grant("SELECT", obj(kinds::TABLE, Some("informix"), "Fact"), "lect", true)), "GRANT SELECT ON informix.\"Fact\" TO lect WITH GRANT OPTION;");
        assert_eq!(s(grant("EXECUTE", obj(kinds::PROCEDURE, None, "p"), "ana", false)), "GRANT EXECUTE ON PROCEDURE p TO ana;");
        assert_eq!(s(SecurityAction::CreateRole { name: "lect".into() }), "CREATE ROLE lect;");
        assert_eq!(s(member("lect", "ana")), "GRANT lect TO ana;");
        assert_eq!(informix::tabauth("Su-idx---"), vec![("SELECT", true), ("UPDATE", false), ("INSERT", false), ("DELETE", false), ("INDEX", false)]);
        assert_eq!(informix::tabauth("s*-------"), vec![("SELECT", false)]);
    }

    #[test]
    fn db2_i_and_z() {
        let (i, z) = (Dialect::Db2i, Dialect::Db2z);
        let s = |d, a| script(d, &a).unwrap();
        for d in [i, z] {
            assert!(refused(d, user_pw("ANA", "x")));
            assert!(refused(d, SecurityAction::SetLogin { name: "ANA".into(), enabled: false }));
            assert!(refused(d, member("R", "ANA")));
        }
        assert!(refused(i, SecurityAction::CreateRole { name: "R".into() }));
        assert_eq!(s(i, grant("SELECT", obj(kinds::TABLE, Some("VENTAS"), "FACT"), "ANA", true)), "GRANT SELECT ON TABLE VENTAS.FACT TO ANA WITH GRANT OPTION;");
        assert_eq!(s(i, grant("EXECUTE", obj(kinds::PROCEDURE, Some("VENTAS"), "P1"), "ANA", false)), "GRANT EXECUTE ON PROCEDURE VENTAS.P1 TO ANA;");
        assert!(script(i, &grant("SELECT", None, "ANA", false)).is_err());
        assert_eq!(s(z, grant("BINDADD", None, "ANA", false)), "GRANT BINDADD TO ANA;");
        assert_eq!(s(z, grant("SELECT", obj(kinds::TABLE, Some("S"), "T"), "role:LECT", false)), "GRANT SELECT ON TABLE S.T TO ROLE LECT;");
        assert_eq!(s(z, grant("CREATEIN", obj("schema", None, "S"), "ANA", false)), "GRANT CREATEIN ON SCHEMA S TO ANA;");
        assert_eq!(s(z, SecurityAction::CreateRole { name: "role:lect".into() }), "CREATE ROLE \"lect\";");
        let p = ibm::i_principal(
            &row(&[
                ("authorization_name", "ANA"),
                ("status", "*DISABLED"),
                ("special_authorities", "*ALLOBJ *JOBCTL"),
                ("group_profile_name", "VENTAS"),
                ("supplemental_group_list", "LECT  AUDIT"),
            ]),
            false,
        );
        assert_eq!((p.disabled, p.superuser, p.member_of.clone()), (Some(true), Some(true), vec!["VENTAS".to_string(), "LECT".into(), "AUDIT".into()]));
        let g = ibm::z_privilege(Grant { privilege: "MON1".into(), ..Default::default() });
        assert_eq!(g.privilege, "MONITOR1");
    }

    #[test]
    fn sybase() {
        let (a, sa) = (Dialect::Ase, Dialect::SqlAnywhere);
        let s = |d, x| script(d, &x).unwrap();
        assert_eq!(s(a, user_pw("ana", "p'w")), "exec sp_addlogin 'ana', 'p''w'\nexec sp_adduser 'ana'");
        assert!(refused(a, SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }));
        assert_eq!(s(a, SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "exec sp_locklogin 'ana', 'lock'");
        assert_eq!(s(a, grant("SELECT", obj(kinds::TABLE, Some("dbo"), "fact x"), "role:lect", true)), "grant select on dbo.[fact x] to lect with grant option");
        assert_eq!(s(a, grant("CREATE TABLE", None, "ana", false)), "grant create table to ana");
        assert_eq!(s(a, member("role:lect", "ana")), "grant role lect to ana");
        assert_eq!(s(a, member("ventas", "ana")), "exec sp_changegroup 'ventas', 'ana'");
        assert_eq!(s(a, SecurityAction::RemoveMember { role: "ventas".into(), member: "ana".into() }), "exec sp_changegroup 'public', 'ana'");
        assert_eq!(s(a, SecurityAction::Drop { name: "ventas".into(), kind: PrincipalKind::Role }), "exec sp_dropgroup 'ventas'");
        assert_eq!(s(a, SecurityAction::Drop { name: "role:lect".into(), kind: PrincipalKind::Role }), "drop role lect");
        assert_eq!(sybase::ase_action("193"), "SELECT");
        assert!(sybase::ase_group(&row(&[("uid", "16390"), ("gid", "16390")])));
        assert!(sybase::ase_group(&row(&[("uid", "0"), ("gid", "0")])));
        assert!(!sybase::ase_group(&row(&[("uid", "5"), ("gid", "16390")])));
        let g = sybase::ase_grant(&row(&[("id", "123"), ("action", "197"), ("protecttype", "2"), ("objname", "t"), ("owner", "dbo"), ("objtype", "U")]), None);
        assert_eq!((g.privilege.as_str(), g.denied, g.object.as_deref()), ("UPDATE", true, Some("dbo.t")));
        let g = sybase::ase_grant(&row(&[("id", "0"), ("action", "198"), ("protecttype", "0")]), None);
        assert_eq!((g.object, g.grantable), (None, true));

        assert_eq!(s(sa, user_pw("ana", "p\"w")), "CREATE USER ana IDENTIFIED BY \"p\"\"w\";");
        assert_eq!(s(sa, SecurityAction::SetLogin { name: "ana".into(), enabled: true }), "ALTER USER ana RESET LOGIN POLICY;");
        assert!(refused(sa, SecurityAction::SetLogin { name: "ana".into(), enabled: false }));
        assert_eq!(s(sa, grant("CREATE ANY TABLE", None, "ana", true)), "GRANT CREATE ANY TABLE TO ana WITH ADMIN OPTION;");
        assert_eq!(s(sa, grant("SELECT", obj(kinds::TABLE, Some("GROUPO"), "Customers"), "lect", false)), "GRANT SELECT ON GROUPO.Customers TO lect;");
        assert_eq!(s(sa, member("lect", "ana")), "GRANT ROLE lect TO ana;");
        assert_eq!(sybase::sa_privilege("SYS_CREATE_ANY_TABLE_ROLE").as_deref(), Some("CREATE ANY TABLE"));
        assert_eq!(sybase::sa_privilege("SYS_AUTH_DBA_ROLE"), None);
        assert_eq!(sybase::sa_privilege("lect"), None);
    }

    #[test]
    fn netezza_and_oracle_like() {
        let n = Dialect::Netezza;
        let s = |d, x| script(d, &x).unwrap();
        assert_eq!(s(n, user_pw("ana", "p'w")), "CREATE USER ANA WITH PASSWORD 'p''w';".replace("ANA", "\"ana\""));
        assert_eq!(s(n, grant("SELECT", obj(kinds::TABLE, Some("ADMIN"), "FACT"), "role:LECT", true)), "GRANT SELECT ON ADMIN.FACT TO GROUP LECT WITH GRANT OPTION;");
        assert_eq!(s(n, grant("CREATE TABLE", None, "ANA", false)), "GRANT CREATE TABLE TO ANA;");
        assert_eq!(s(n, member("role:LECT", "ANA")), "ALTER GROUP LECT ADD USER ANA;");
        assert_eq!(s(n, SecurityAction::CreateRole { name: "LECT".into() }), "CREATE GROUP LECT;");
        assert!(refused(n, SecurityAction::SetLogin { name: "ANA".into(), enabled: false }));
        let g = netezza::grants_of(&row(&[("objid", "200"), ("priv", "6"), ("gpriv", "2"), ("objname", "FACT"), ("objtype", "TABLE"), ("dbname", "VENTAS"), ("objschema", "ADMIN")]), None);
        let names: Vec<(&str, bool)> = g.iter().map(|g| (g.privilege.as_str(), g.grantable)).collect();
        assert_eq!(names, vec![("SELECT", true), ("INSERT", false)]);
        assert_eq!(g[0].object.as_deref(), Some("VENTAS.ADMIN.FACT"));

        for d in [Dialect::Altibase, Dialect::Dameng] {
            assert_eq!(s(d, user_pw("ana", "Pw1")), "CREATE USER \"ana\" IDENTIFIED BY \"Pw1\";");
            assert_eq!(s(d, SecurityAction::SetLogin { name: "ANA".into(), enabled: false }), "ALTER USER ANA ACCOUNT LOCK;");
            assert_eq!(s(d, grant("SELECT", obj(kinds::TABLE, Some("SYS"), "T"), "ANA", true)), "GRANT SELECT ON SYS.T TO ANA WITH GRANT OPTION;");
            assert_eq!(s(d, member("R", "ANA")), "GRANT R TO ANA;");
        }
        assert_eq!(s(Dialect::Dameng, grant("CREATE TABLE", None, "ANA", true)), "GRANT CREATE TABLE TO ANA WITH ADMIN OPTION;");
        assert!(refused(Dialect::Altibase, grant("CREATE TABLE", None, "ANA", true)));
    }

    #[test]
    fn groups_engines() {
        let s = |d, x| script(d, &x).unwrap();
        let (c, z, m) = (Dialect::Cubrid, Dialect::Zen, Dialect::Mimer);
        assert_eq!(s(c, user_pw("ana", "x")), "CREATE USER ana PASSWORD 'x';");
        assert_eq!(s(c, SecurityAction::CreateRole { name: "ventas".into() }), "CREATE USER ventas;");
        assert_eq!(s(c, member("ventas", "ana")), "ALTER USER ventas ADD MEMBERS ana;");
        assert_eq!(s(c, grant("SELECT", obj(kinds::TABLE, None, "fact x"), "ana", true)), "GRANT SELECT ON [fact x] TO ana WITH GRANT OPTION;");
        assert!(script(c, &grant("SELECT", None, "ana", false)).is_err());
        assert_eq!(s(z, user_pw("ana", "x")), "CREATE USER ana WITH PASSWORD \"x\";");
        assert_eq!(s(z, member("ventas", "ana")), "ALTER GROUP ventas ADD USER ana;");
        assert_eq!(s(z, grant("CREATETAB", None, "ana", false)), "GRANT CREATETAB TO ana;");
        assert!(refused(z, grant("SELECT", obj(kinds::TABLE, None, "t"), "ana", true)));
        assert_eq!(s(m, user_pw("ANA", "x")), "CREATE IDENT ANA AS USER USING 'x';");
        assert_eq!(s(m, SecurityAction::SetPassword { name: "ANA".into(), password: "y".into() }), "ALTER IDENT ANA SET PASSWORD 'y';");
        assert_eq!(s(m, member("VENTAS", "ANA")), "GRANT MEMBER ON VENTAS TO ANA;");
        assert_eq!(s(m, grant("BACKUP", None, "ANA", false)), "GRANT BACKUP TO ANA;");
        for d in [c, z, m] {
            assert!(refused(d, SecurityAction::SetLogin { name: "a".into(), enabled: false }));
        }
        assert_eq!(groups::zen_rights(0x40 | 0x80 | 0x04 | 0x08), vec!["SELECT", "INSERT", "DELETE"]);
        assert_eq!(groups::zen_rights(0x40), vec!["SELECT"]);
        let g = groups::cubrid_grant(&row(&[("grantee_name", "ANA"), ("owner_name", "DBA"), ("class_name", "t"), ("auth_type", "SELECT"), ("is_grantable", "YES")]), None);
        assert_eq!((g.object.as_deref(), g.grantable), (Some("DBA.t"), true));
    }

    #[test]
    fn standard_ones() {
        let s = |d, x| script(d, &x).unwrap();
        assert_eq!(s(Dialect::MonetDb, user_pw("ana", "x")), "CREATE USER ana WITH PASSWORD 'x' NAME 'ana' SCHEMA \"sys\";");
        assert_eq!(s(Dialect::MonetDb, grant("COPY FROM", None, "ana", false)), "GRANT COPY FROM TO ana;");
        assert_eq!(s(Dialect::MonetDb, grant("EXECUTE", obj(kinds::FUNCTION, Some("sys"), "f"), "ana", false)), "GRANT EXECUTE ON FUNCTION sys.f TO ana;");
        assert_eq!(s(Dialect::Ingres, grant("SELECT", obj(kinds::TABLE, None, "t"), "role:lect", true)), "GRANT SELECT ON TABLE t TO ROLE lect WITH GRANT OPTION;");
        assert_eq!(s(Dialect::Ingres, user_pw("ana", "x")), "CREATE USER ana WITH PASSWORD = 'x';");
        assert_eq!(s(Dialect::Iris, grant("%CREATE_TABLE", None, "ana", true)), "GRANT %CREATE_TABLE TO ana WITH ADMIN OPTION;");
        assert_eq!(s(Dialect::Iris, grant("SELECT", obj("schema", None, "Sample"), "ana", false)), "GRANT SELECT ON SCHEMA Sample TO ana;");
        assert_eq!(s(Dialect::MaxDb, SecurityAction::SetLogin { name: "ANA".into(), enabled: false }), "ALTER USER ANA DISABLE CONNECT;");
        assert_eq!(s(Dialect::MaxDb, SecurityAction::SetPassword { name: "ANA".into(), password: "x".into() }), "ALTER PASSWORD ANA \"x\";");
        assert_eq!(s(Dialect::MaxDb, grant("CREATEIN", obj("schema", None, "S"), "ANA", false)), "GRANT CREATEIN ON S TO ANA;");
        assert_eq!(s(Dialect::NuoDb, SecurityAction::CreateRole { name: "role:USER.LECT".into() }), "CREATE ROLE \"USER\".LECT;");
        assert_eq!(s(Dialect::NuoDb, member("role:USER.LECT", "ANA")), "GRANT \"USER\".LECT TO ANA;");
        assert_eq!(s(Dialect::NuoDb, grant("SELECT", obj(kinds::TABLE, Some("S"), "T"), "role:USER.LECT", false)), "GRANT SELECT ON TABLE S.T TO ROLE \"USER\".LECT;");
        assert_eq!(s(Dialect::HeavyDb, user_pw("ana", "x")), "CREATE USER ana (password = 'x');");
        assert_eq!(s(Dialect::HeavyDb, SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ALTER USER ana (can_login = 'false');");
        assert!(refused(Dialect::HeavyDb, grant("SELECT", obj(kinds::TABLE, None, "t"), "ana", true)));
        assert_eq!(s(Dialect::Sqream, user_pw("ana", "x")), "CREATE ROLE ana;\nGRANT LOGIN TO ana;\nGRANT PASSWORD 'x' TO ana;");
        assert_eq!(s(Dialect::Sqream, grant("USAGE", obj("schema", None, "public"), "ana", false)), "GRANT USAGE ON SCHEMA public TO ana;");
        assert_eq!(s(Dialect::Sqream, SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "REVOKE LOGIN FROM ana;");
        assert!(refused(Dialect::Ingres, SecurityAction::SetLogin { name: "a".into(), enabled: false }));
        let g = sqlstd::monet_grants_of(&row(&[("privileges", "5"), ("grantable", "1"), ("tname", "t"), ("tschema", "sys"), ("ttype", "0")]), &None);
        assert_eq!(g.iter().map(|g| g.privilege.as_str()).collect::<Vec<_>>(), vec!["SELECT", "INSERT"]);
        assert!(g.iter().all(|g| g.grantable && g.object.as_deref() == Some("sys.t")));
        let g = sqlstd::nuo_grants_of(&row(&[("privilegemask", &(2 | 4 | 2048).to_string()), ("objecttype", "0"), ("objectschema", "S"), ("objectname", "T")]), &None);
        assert_eq!(g.iter().map(|g| (g.privilege.as_str(), g.grantable)).collect::<Vec<_>>(), vec![("SELECT", true), ("INSERT", false)]);
    }

    /// Multi-statement security scripts, as `Session::execute` sends them.
    #[test]
    fn multi_statement_scripts_split_as_execute_sends_them() {
        let sent = |id: &str, d: Dialect, a: SecurityAction| -> Vec<String> {
            let p = crate::PRESETS.iter().find(|p| p.id == id).unwrap();
            crate::split(p.batch, &script(d, &a).unwrap()).into_iter().map(|s| s.trim().to_string()).collect()
        };
        // ASE: one GO batch with both procedure calls.
        assert_eq!(sent("sybase", Dialect::Ase, user_pw("ana", "x")), vec!["exec sp_addlogin 'ana', 'x'\nexec sp_adduser 'ana'"]);
        assert_eq!(
            sent("teradata", Dialect::Teradata, SecurityAction::SetLogin { name: "ana".into(), enabled: true }),
            vec!["MODIFY USER ana AS RELEASE PASSWORD LOCK", "GRANT LOGON ON ALL TO ana"]
        );
        assert_eq!(sent("sqream", Dialect::Sqream, user_pw("ana", "x")), vec!["CREATE ROLE ana", "GRANT LOGIN TO ana", "GRANT PASSWORD 'x' TO ana"]);
        // A `;` inside a literal doesn't split.
        assert_eq!(sent("virtuoso", Dialect::Virtuoso, user_pw("ana", "a;b")), vec!["DB.DBA.USER_CREATE('ana', 'a;b')"]);
    }

    #[test]
    fn misc_engines() {
        let s = |d, x| script(d, &x).unwrap();
        let v = Dialect::Virtuoso;
        assert_eq!(s(v, user_pw("ana", "p'w")), "DB.DBA.USER_CREATE('ana', 'p''w');");
        assert_eq!(s(v, SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "DB.DBA.USER_SET_OPTION('ana', 'DISABLED', 1);");
        assert_eq!(s(v, SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), "DB.DBA.USER_DROP('ana');");
        assert_eq!(s(v, member("lect", "ana")), "GRANT lect TO ana;");
        let g = misc::virtuoso_grants_of(&row(&[("g_op", "17"), ("g_object", "DB.DBA.t"), ("g_col", "_")]), &None);
        assert_eq!((g.len(), g[0].privilege.as_str(), g[0].grantable), (1, "SELECT", true));
        let o = Dialect::OpenEdge;
        assert_eq!(s(o, user_pw("ana", "x")), "CREATE USER 'ana', 'x';");
        assert_eq!(s(o, grant("DBA", None, "ana", false)), "GRANT DBA TO ana;");
        assert!(refused(o, grant("DBA", None, "ana", true)));
        assert!(refused(o, SecurityAction::CreateRole { name: "r".into() }));
        assert_eq!(misc::openedge_flag("g"), Some(true));
        assert_eq!(misc::openedge_flag("y"), Some(false));
        assert_eq!(misc::openedge_flag("n"), None);
        assert_eq!(s(Dialect::Machbase, user_pw("ana", "x")), "CREATE USER ana IDENTIFIED BY 'x';");
        assert_eq!(s(Dialect::Ignite, user_pw("ANA", "x")), "CREATE USER ANA WITH PASSWORD 'x';");
        assert!(refused(Dialect::Ignite, grant("SELECT", obj(kinds::TABLE, None, "T"), "ANA", false)));
        let oc = Dialect::Ocient;
        assert_eq!(s(oc, grant("SELECT", obj(kinds::TABLE, Some("s"), "t"), "group:analistas", true)), "GRANT SELECT ON TABLE s.t TO GROUP analistas WITH GRANT OPTION;");
        assert_eq!(s(oc, grant("CREATE DATABASE", None, "ana", false)), "GRANT CREATE DATABASE ON SYSTEM TO USER ana;");
        assert_eq!(s(oc, member("group:analistas", "ana")), "ALTER GROUP analistas ADD USER ana;");
        assert_eq!(s(oc, member("role:System Administrator", "ana")), "GRANT ROLE \"System Administrator\" TO USER ana;");
        assert_eq!(s(oc, SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ALTER USER ana DISABLE;");
        assert!(refused(oc, SecurityAction::Drop { name: "role:System Administrator".into(), kind: PrincipalKind::Role }));
    }
}

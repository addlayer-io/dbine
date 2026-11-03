//! What each preset's GRANT accepts, and on what (`Session::privileges`).
//!
//! DBine has no server of these engines to check a catalog against, so the
//! lists come from each engine's docs (cited on each table): the names as
//! its GRANT takes them, each on the kinds of [`super::spec`] it can be
//! granted on ("" = the whole database or server). Privileges that only go
//! on objects DBine doesn't offer for that engine (sequences, macros,
//! connections, domains, directories, users…) are left out.

use super::Dialect;
use dbine_driver::kinds::{FUNCTION as F, PROCEDURE as P, TABLE as T, VIEW as V};
use dbine_driver::Privilege;

/// The whole database / server.
const S: &str = "";
const SC: &str = "schema";

type Objects = &'static [(&'static str, &'static [&'static str])];

/// The privileges `d`'s GRANT accepts, per kind. Ignite 2 has no GRANT.
pub fn grantable(d: Dialect) -> Vec<Privilege> {
    let (objects, system): (Objects, &[&str]) = match d {
        Dialect::Hive => (HIVE, &[]),
        Dialect::Impala => (IMPALA, &[]),
        Dialect::Vertica => (VERTICA, &[]),
        Dialect::Exasol => (EXASOL, EXASOL_SYSTEM),
        Dialect::Db2 => (DB2, DB2_DATABASE),
        Dialect::Teradata => (TERADATA, &[]),
        Dialect::Informix => (INFORMIX, &["CONNECT", "RESOURCE", "DBA"]),
        Dialect::Db2i => (DB2I, &[]),
        Dialect::Db2z => (DB2Z, DB2Z_SYSTEM),
        Dialect::Ase => (ASE, ASE_COMMANDS),
        Dialect::SqlAnywhere => (SQL_ANYWHERE, SQL_ANYWHERE_SYSTEM),
        Dialect::Netezza => (NETEZZA, NETEZZA_ADMIN),
        Dialect::Altibase => (ALTIBASE, ALTIBASE_SYSTEM),
        Dialect::Dameng => (DAMENG, DAMENG_SYSTEM),
        Dialect::Cubrid => (CUBRID, &[]),
        Dialect::Zen => (ZEN, &["CREATETAB", "CREATEVIEW", "CREATESP"]),
        Dialect::Mimer => (
            MIMER,
            &[
                "BACKUP",
                "DATABANK",
                "IDENT",
                "SCHEMA",
                "SHADOW",
                "STATISTICS",
            ],
        ),
        Dialect::MonetDb => (MONETDB, &["COPY FROM", "COPY INTO"]),
        Dialect::Ingres => (INGRES, &[]),
        Dialect::Iris => (IRIS, IRIS_ADMIN),
        Dialect::MaxDb => (MAXDB, &[]),
        Dialect::NuoDb => (NUODB, &[]),
        Dialect::HeavyDb => (HEAVYDB, &[]),
        Dialect::Sqream => (SQREAM, &["LOGIN"]),
        Dialect::Virtuoso => (VIRTUOSO, &[]),
        Dialect::OpenEdge => (
            OPENEDGE,
            &[
                "DBA",
                "RESOURCE",
                "AUDIT_ADMIN",
                "AUDIT_ARCHIVE",
                "AUDIT_INSERT",
            ],
        ),
        Dialect::Machbase => (MACHBASE, &[]),
        Dialect::Ignite => (&[], &[]),
        Dialect::Ocient => (OCIENT, OCIENT_SYSTEM),
    };
    objects
        .iter()
        .map(|(name, on)| Privilege::new(*name, on))
        .chain(system.iter().map(|name| Privilege::new(*name, &[S])))
        .collect()
}

/// SQL standard based authorization grants on tables and views; databases
/// (`ON DATABASE`) take them with Ranger.
/// <https://cwiki.apache.org/confluence/display/Hive/SQL+Standard+Based+Hive+Authorization>
const HIVE: Objects = &[
    ("SELECT", &[SC, T, V]),
    ("INSERT", &[SC, T]),
    ("UPDATE", &[SC, T]),
    ("DELETE", &[SC, T]),
    ("ALL", &[SC, T, V]),
];

/// <https://impala.apache.org/docs/build/html/topics/impala_authorization.html>
const IMPALA: Objects = &[
    ("SELECT", &[S, SC, T, V]),
    ("INSERT", &[S, SC, T]),
    ("ALTER", &[S, SC, T, V]),
    ("CREATE", &[S, SC]),
    ("DROP", &[S, SC, T, V]),
    ("REFRESH", &[S, SC, T]),
    ("ALL", &[S, SC, T, V]),
];

/// GRANT (schema), (table) and (view):
/// <https://docs.vertica.com/latest/en/sql-reference/statements/grant-statements/>
const VERTICA: Objects = &[
    ("SELECT", &[SC, T, V]),
    ("INSERT", &[SC, T]),
    ("UPDATE", &[SC, T]),
    ("DELETE", &[SC, T]),
    ("REFERENCES", &[SC, T]),
    ("TRUNCATE", &[SC, T]),
    ("ALTER", &[SC, T, V]),
    ("DROP", &[SC, T, V]),
    ("USAGE", &[SC]),
    ("CREATE", &[SC]),
    ("ALL", &[SC, T, V]),
];

/// <https://docs.exasol.com/db/latest/database_concepts/privileges/details_rights_management.htm>
const EXASOL: Objects = &[
    ("SELECT", &[SC, T, V]),
    ("INSERT", &[SC, T]),
    ("UPDATE", &[SC, T]),
    ("DELETE", &[SC, T]),
    ("ALTER", &[SC, T]),
    ("REFERENCES", &[T]),
    ("EXECUTE", &[SC]),
    ("USAGE", &[SC]),
    ("ALL", &[S, SC, T, V]),
];

const EXASOL_SYSTEM: &[&str] = &[
    "CREATE SESSION",
    "KILL ANY SESSION",
    "ALTER SYSTEM",
    "CREATE USER",
    "ALTER USER",
    "DROP USER",
    "IMPERSONATE ANY USER",
    "CREATE ROLE",
    "DROP ANY ROLE",
    "GRANT ANY ROLE",
    "CREATE CONNECTION",
    "ALTER ANY CONNECTION",
    "DROP ANY CONNECTION",
    "GRANT ANY CONNECTION",
    "USE ANY CONNECTION",
    "ACCESS ANY CONNECTION",
    "CREATE SCHEMA",
    "ALTER ANY SCHEMA",
    "DROP ANY SCHEMA",
    "USE ANY SCHEMA",
    "CREATE VIRTUAL SCHEMA",
    "ALTER ANY VIRTUAL SCHEMA",
    "ALTER ANY VIRTUAL SCHEMA REFRESH",
    "DROP ANY VIRTUAL SCHEMA",
    "CREATE TABLE",
    "CREATE ANY TABLE",
    "ALTER ANY TABLE",
    "DELETE ANY TABLE",
    "DROP ANY TABLE",
    "INSERT ANY TABLE",
    "SELECT ANY TABLE",
    "SELECT ANY DICTIONARY",
    "UPDATE ANY TABLE",
    "CREATE VIEW",
    "CREATE ANY VIEW",
    "DROP ANY VIEW",
    "CREATE FUNCTION",
    "CREATE ANY FUNCTION",
    "DROP ANY FUNCTION",
    "EXECUTE ANY FUNCTION",
    "CREATE SCRIPT",
    "CREATE ANY SCRIPT",
    "DROP ANY SCRIPT",
    "EXECUTE ANY SCRIPT",
    "IMPORT",
    "EXPORT",
    "GRANT ANY OBJECT PRIVILEGE",
    "GRANT ANY PRIVILEGE",
    "MANAGE CONSUMER GROUPS",
    "SET ANY CONSUMER GROUP",
];

/// Db2 11.5 for LUW: GRANT (database authorities), (schema privileges) and
/// (table, view, or nickname privileges).
/// <https://www.ibm.com/docs/en/db2/11.5.x?topic=statements-grant-database-authorities>
const DB2: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("ALTER", &[T]),
    ("INDEX", &[T]),
    ("REFERENCES", &[T]),
    ("CONTROL", &[T, V]),
    ("ALL", &[T, V]),
    ("CREATEIN", &[SC]),
    ("ALTERIN", &[SC]),
    ("DROPIN", &[SC]),
    ("SELECTIN", &[SC]),
    ("INSERTIN", &[SC]),
    ("UPDATEIN", &[SC]),
    ("DELETEIN", &[SC]),
    ("EXECUTEIN", &[SC]),
    ("SCHEMAADM", &[SC]),
    ("ACCESSCTRL", &[S, SC]),
    ("DATAACCESS", &[S, SC]),
    ("LOAD", &[S, SC]),
];

const DB2_DATABASE: &[&str] = &[
    "CONNECT",
    "CREATETAB",
    "BINDADD",
    "IMPLICIT_SCHEMA",
    "CREATE_EXTERNAL_ROUTINE",
    "CREATE_NOT_FENCED_ROUTINE",
    "CREATE_SECURE_OBJECT",
    "EXPLAIN",
    "QUIESCE_CONNECT",
    "DBADM",
    "SECADM",
    "SQLADM",
    "WLMADM",
];

/// Teradata has no rights on the whole system by GRANT (SQL form); the
/// ones on a database (DBine's schema) cover its objects. Macros, roles,
/// profiles, zones and UDTs aren't offered.
/// <https://docs.teradata.com/r/Enterprise_IntelliFlex_VMware/SQL-Data-Control-Language/Statement-Syntax/GRANT-SQL-Form>
const TERADATA: Objects = &[
    ("SELECT", &[SC, T, V]),
    ("INSERT", &[SC, T, V]),
    ("UPDATE", &[SC, T, V]),
    ("DELETE", &[SC, T, V]),
    ("REFERENCES", &[SC, T]),
    ("INDEX", &[SC, T]),
    ("EXECUTE", &[SC]),
    ("EXECUTE PROCEDURE", &[SC, P]),
    ("EXECUTE FUNCTION", &[SC, F]),
    ("CREATE TABLE", &[SC]),
    ("CREATE VIEW", &[SC]),
    ("CREATE MACRO", &[SC]),
    ("CREATE PROCEDURE", &[SC]),
    ("CREATE EXTERNAL PROCEDURE", &[SC]),
    ("CREATE FUNCTION", &[SC]),
    ("CREATE TRIGGER", &[SC, T]),
    ("CREATE DATABASE", &[SC]),
    ("CREATE USER", &[SC]),
    ("CREATE AUTHORIZATION", &[SC]),
    ("DROP TABLE", &[SC, T]),
    ("DROP VIEW", &[SC, V]),
    ("DROP MACRO", &[SC]),
    ("DROP PROCEDURE", &[SC, P]),
    ("DROP FUNCTION", &[SC, F]),
    ("DROP TRIGGER", &[SC, T]),
    ("DROP DATABASE", &[SC]),
    ("DROP USER", &[SC]),
    ("DROP AUTHORIZATION", &[SC]),
    ("ALTER PROCEDURE", &[SC, P]),
    ("ALTER EXTERNAL PROCEDURE", &[SC, P]),
    ("ALTER FUNCTION", &[SC, F]),
    ("DUMP", &[SC, T]),
    ("RESTORE", &[SC, T]),
    ("CHECKPOINT", &[SC, T]),
    ("SHOW", &[SC, T, V, P, F]),
    ("STATISTICS", &[SC, T]),
    ("NONTEMPORAL", &[SC, T]),
    ("ALL", &[SC, T, V, P, F]),
];

/// Database-level (CONNECT, RESOURCE, DBA), table, view and routine
/// privileges. <https://www.ibm.com/docs/en/informix-servers/14.10?topic=statements-grant-statement>
const INFORMIX: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("INDEX", &[T]),
    ("ALTER", &[T]),
    ("REFERENCES", &[T]),
    ("UNDER", &[T]),
    ("EXECUTE", &[P, F]),
    ("ALL", &[T, V]),
];

/// GRANT (table or view privileges) and (function or procedure
/// privileges). <https://www.ibm.com/docs/en/i/7.5?topic=statements-grant-table-view-privileges>
const DB2I: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("ALTER", &[T, V, P, F]),
    ("INDEX", &[T]),
    ("REFERENCES", &[T]),
    ("EXECUTE", &[P, F]),
    ("ALL", &[T, V, P, F]),
];

/// Db2 13 for z/OS: GRANT (system privileges), (schema privileges) and
/// (table or view privileges).
/// <https://www.ibm.com/docs/en/db2-for-zos/13?topic=statements-grant-system-privileges>
const DB2Z: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("ALTER", &[T]),
    ("INDEX", &[T]),
    ("REFERENCES", &[T]),
    ("TRIGGER", &[T]),
    ("UNLOAD", &[T]),
    ("ALL", &[T, V]),
    ("CREATEIN", &[SC]),
    ("ALTERIN", &[SC]),
    ("DROPIN", &[SC]),
];

const DB2Z_SYSTEM: &[&str] = &[
    "ACCESSCTRL",
    "ARCHIVE",
    "BINDADD",
    "BINDAGENT",
    "BSDS",
    "CREATEALIAS",
    "CREATEDBA",
    "CREATEDBC",
    "CREATESG",
    "CREATETMTABLE",
    "CREATE_SECURE_OBJECT",
    "DATAACCESS",
    "DEBUGSESSION",
    "DISPLAY",
    "EXPLAIN",
    "MONITOR1",
    "MONITOR2",
    "RECOVER",
    "SQLADM",
    "STOPALL",
    "STOSPACE",
    "SYSADM",
    "SYSCTRL",
    "SYSOPR",
    "TRACE",
];

/// SAP ASE Reference Manual, Commands, `grant`: object permissions and
/// the database's command permissions (CREATE DATABASE only in master;
/// the granular ones need `enable granular permissions`, so they're left
/// out). <https://help.sap.com/docs/SAP_ASE>
const ASE: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("REFERENCES", &[T]),
    ("TRUNCATE TABLE", &[T]),
    ("UPDATE STATISTICS", &[T]),
    ("DELETE STATISTICS", &[T]),
    ("TRANSFER TABLE", &[T]),
    ("DECRYPT", &[T]),
    ("EXECUTE", &[P, F]),
    ("ALL", &[S, T, V, P, F]),
];

const ASE_COMMANDS: &[&str] = &[
    "CREATE DATABASE",
    "CREATE DEFAULT",
    "CREATE FUNCTION",
    "CREATE PROCEDURE",
    "CREATE RULE",
    "CREATE TABLE",
    "CREATE TRIGGER",
    "CREATE VIEW",
    "DUMP DATABASE",
    "DUMP TRANSACTION",
    "SET PROXY",
    "SET SESSION AUTHORIZATION",
];

/// SQL Anywhere 17 SQL Reference: "GRANT object-level privilege
/// statement" and "List of all system privileges".
/// <https://help.sap.com/docs/SAP_SQL_Anywhere>
const SQL_ANYWHERE: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("ALTER", &[T]),
    ("REFERENCES", &[T]),
    ("LOAD", &[T]),
    ("TRUNCATE", &[T]),
    ("EXECUTE", &[P, F]),
    ("ALL", &[T, V]),
];

const SQL_ANYWHERE_SYSTEM: &[&str] = &[
    "ACCESS USER PASSWORD",
    "ALTER ANY INDEX",
    "ALTER ANY MATERIALIZED VIEW",
    "ALTER ANY OBJECT",
    "ALTER ANY OBJECT OWNER",
    "ALTER ANY PROCEDURE",
    "ALTER ANY SEQUENCE",
    "ALTER ANY TABLE",
    "ALTER ANY TEXT CONFIGURATION",
    "ALTER ANY TRIGGER",
    "ALTER ANY VIEW",
    "ALTER DATABASE",
    "ALTER DATATYPE",
    "BACKUP DATABASE",
    "CHANGE PASSWORD",
    "CHECKPOINT",
    "COMMENT ANY OBJECT",
    "CREATE ANY INDEX",
    "CREATE ANY MATERIALIZED VIEW",
    "CREATE ANY MUTEX SEMAPHORE",
    "CREATE ANY OBJECT",
    "CREATE ANY PROCEDURE",
    "CREATE ANY SEQUENCE",
    "CREATE ANY TABLE",
    "CREATE ANY TEXT CONFIGURATION",
    "CREATE ANY TRIGGER",
    "CREATE ANY VIEW",
    "CREATE DATATYPE",
    "CREATE EXTERNAL REFERENCE",
    "CREATE MATERIALIZED VIEW",
    "CREATE MESSAGE",
    "CREATE PROCEDURE",
    "CREATE PROXY TABLE",
    "CREATE TABLE",
    "CREATE TEXT CONFIGURATION",
    "CREATE VIEW",
    "DEBUG ANY PROCEDURE",
    "DELETE ANY TABLE",
    "DROP ANY INDEX",
    "DROP ANY MATERIALIZED VIEW",
    "DROP ANY MUTEX SEMAPHORE",
    "DROP ANY OBJECT",
    "DROP ANY PROCEDURE",
    "DROP ANY SEQUENCE",
    "DROP ANY TABLE",
    "DROP ANY TEXT CONFIGURATION",
    "DROP ANY VIEW",
    "DROP CONNECTION",
    "DROP DATATYPE",
    "DROP MESSAGE",
    "EXECUTE ANY PROCEDURE",
    "INSERT ANY TABLE",
    "LOAD ANY TABLE",
    "MANAGE ANY DBSPACE",
    "MANAGE ANY EVENT",
    "MANAGE ANY EXTERNAL ENVIRONMENT",
    "MANAGE ANY EXTERNAL OBJECT",
    "MANAGE ANY LDAP SERVER",
    "MANAGE ANY LOGIN POLICY",
    "MANAGE ANY MIRROR SERVER",
    "MANAGE ANY OBJECT PRIVILEGE",
    "MANAGE ANY SPATIAL OBJECT",
    "MANAGE ANY STATISTICS",
    "MANAGE ANY USER",
    "MANAGE ANY WEB SERVICE",
    "MANAGE AUDITING",
    "MANAGE PROFILING",
    "MANAGE REPLICATION",
    "MANAGE ROLES",
    "MONITOR",
    "READ CLIENT FILE",
    "READ FILE",
    "REORGANIZE ANY OBJECT",
    "SELECT ANY TABLE",
    "SERVER OPERATOR",
    "SET ANY PUBLIC OPTION",
    "SET ANY SECURITY OPTION",
    "SET ANY SYSTEM OPTION",
    "SET ANY USER DEFINED OPTION",
    "SET USER",
    "TRUNCATE ANY TABLE",
    "UPDATE ANY MUTEX SEMAPHORE",
    "UPDATE ANY TABLE",
    "UPGRADE ROLE",
    "USE ANY SEQUENCE",
    "VALIDATE ANY OBJECT",
    "WRITE CLIENT FILE",
    "WRITE FILE",
];

/// Object and administration privileges of GRANT (CREATE INDEX is for
/// system use only). <https://www.ibm.com/docs/en/netezza?topic=npsscr-grant-2>
const NETEZZA: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T]),
    ("UPDATE", &[T]),
    ("DELETE", &[T]),
    ("TRUNCATE", &[T]),
    ("LOCK", &[T]),
    ("ALTER", &[T, V]),
    ("DROP", &[T, V]),
    ("LIST", &[T, V]),
    ("LOAD", &[T]),
    ("GENSTATS", &[T]),
    ("GROOM", &[T]),
    ("ALL", &[T, V]),
];

const NETEZZA_ADMIN: &[&str] = &[
    "BACKUP",
    "RESTORE",
    "CREATE AGGREGATE",
    "CREATE DATABASE",
    "CREATE EXTERNAL TABLE",
    "CREATE FUNCTION",
    "CREATE GROUP",
    "CREATE LIBRARY",
    "CREATE MATERIALIZED VIEW",
    "CREATE PROCEDURE",
    "CREATE SCHEMA",
    "CREATE SEQUENCE",
    "CREATE SYNONYM",
    "CREATE TABLE",
    "CREATE TEMP TABLE",
    "CREATE USER",
    "CREATE VIEW",
    "MANAGE HARDWARE",
    "MANAGE SECURITY",
    "MANAGE SYSTEM",
    "UNFENCE",
    "ALL ADMIN",
];

/// Altibase 7.3, GRANT: the system privileges SYSTEM_.SYS_PRIVILEGES_
/// lists, without the SYS-only ones (ALTER DATABASE, DROP DATABASE,
/// MANAGE TABLESPACE, SYSDBA), and the object privileges per object.
/// <https://github.com/ALTIBASE/Documents/blob/master/Manuals/Altibase_7.3/eng/SQL%20Reference.md#grant>
const ALTIBASE: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T]),
    ("UPDATE", &[T]),
    ("DELETE", &[T]),
    ("ALTER", &[T]),
    ("INDEX", &[T]),
    ("REFERENCES", &[T]),
    ("EXECUTE", &[P, F]),
    ("ALL PRIVILEGES", &[S, T, V, P, F]),
];

const ALTIBASE_SYSTEM: &[&str] = &[
    "CREATE SESSION",
    "ALTER SYSTEM",
    "CREATE TABLE",
    "CREATE ANY TABLE",
    "ALTER ANY TABLE",
    "DELETE ANY TABLE",
    "DROP ANY TABLE",
    "INSERT ANY TABLE",
    "LOCK ANY TABLE",
    "SELECT ANY TABLE",
    "UPDATE ANY TABLE",
    "CREATE VIEW",
    "CREATE ANY VIEW",
    "DROP ANY VIEW",
    "CREATE ANY INDEX",
    "ALTER ANY INDEX",
    "DROP ANY INDEX",
    "CREATE PROCEDURE",
    "CREATE ANY PROCEDURE",
    "ALTER ANY PROCEDURE",
    "DROP ANY PROCEDURE",
    "EXECUTE ANY PROCEDURE",
    "CREATE SEQUENCE",
    "CREATE ANY SEQUENCE",
    "ALTER ANY SEQUENCE",
    "DROP ANY SEQUENCE",
    "SELECT ANY SEQUENCE",
    "CREATE TRIGGER",
    "CREATE ANY TRIGGER",
    "ALTER ANY TRIGGER",
    "DROP ANY TRIGGER",
    "CREATE SYNONYM",
    "CREATE PUBLIC SYNONYM",
    "CREATE ANY SYNONYM",
    "DROP ANY SYNONYM",
    "DROP PUBLIC SYNONYM",
    "CREATE MATERIALIZED VIEW",
    "CREATE ANY MATERIALIZED VIEW",
    "ALTER ANY MATERIALIZED VIEW",
    "DROP ANY MATERIALIZED VIEW",
    "CREATE LIBRARY",
    "CREATE ANY LIBRARY",
    "ALTER ANY LIBRARY",
    "DROP ANY LIBRARY",
    "CREATE ANY DIRECTORY",
    "DROP ANY DIRECTORY",
    "CREATE DATABASE LINK",
    "CREATE PUBLIC DATABASE LINK",
    "DROP PUBLIC DATABASE LINK",
    "CREATE ANY JOB",
    "ALTER ANY JOB",
    "DROP ANY JOB",
    "CREATE TABLESPACE",
    "ALTER TABLESPACE",
    "DROP TABLESPACE",
    "CREATE USER",
    "ALTER USER",
    "DROP USER",
    "CREATE ROLE",
    "DROP ANY ROLE",
    "GRANT ANY ROLE",
    "GRANT ANY PRIVILEGES",
];

/// DM8 Security Management manual, database (system) and object
/// privileges. <https://eco.dameng.com/document/dm/en/pm/>
const DAMENG: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("ALTER", &[T]),
    ("INDEX", &[T]),
    ("REFERENCES", &[T]),
    ("EXECUTE", &[P, F]),
    ("ALL", &[T, V, P, F]),
];

const DAMENG_SYSTEM: &[&str] = &[
    "CREATE SESSION",
    "CREATE SCHEMA",
    "CREATE TABLE",
    "CREATE VIEW",
    "CREATE INDEX",
    "CREATE PROCEDURE",
    "CREATE SEQUENCE",
    "CREATE TRIGGER",
    "CREATE SYNONYM",
    "CREATE ANY TABLE",
    "ALTER ANY TABLE",
    "DROP ANY TABLE",
    "INSERT ANY TABLE",
    "UPDATE ANY TABLE",
    "DELETE ANY TABLE",
    "SELECT ANY TABLE",
    "CREATE ANY VIEW",
    "DROP ANY VIEW",
    "CREATE ANY INDEX",
    "DROP ANY INDEX",
    "CREATE ANY PROCEDURE",
    "DROP ANY PROCEDURE",
    "EXECUTE ANY PROCEDURE",
    "CREATE ANY SEQUENCE",
    "DROP ANY SEQUENCE",
    "SELECT ANY SEQUENCE",
    "CREATE ANY TRIGGER",
    "DROP ANY TRIGGER",
    "CREATE USER",
    "ALTER USER",
    "DROP USER",
    "CREATE ROLE",
    "ALTER DATABASE",
    "BACKUP DATABASE",
];

/// CUBRID has no system privileges by GRANT.
/// <https://www.cubrid.org/manual/en/11.2/sql/authorization.html>
const CUBRID: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("ALTER", &[T, V]),
    ("INDEX", &[T]),
    ("EXECUTE", &[P]),
    ("ALL PRIVILEGES", &[T, V]),
];

/// Actian Zen: database rights (CREATETAB, CREATEVIEW, CREATESP) and
/// table, view and procedure rights.
/// <https://docs.actian.com/zen/v15/index.html#page/sqlref/sqlref.GRANT.htm>
const ZEN: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("ALTER", &[T, V, P]),
    ("REFERENCES", &[T]),
    ("EXECUTE", &[P]),
    ("ALL", &[T, V, P]),
];

/// Mimer SQL: GRANT ACCESS PRIVILEGE (tables, views), OBJECT PRIVILEGE
/// (EXECUTE on routines; USAGE goes on domains, sequences and types) and
/// SYSTEM PRIVILEGE.
/// (Mimer SQL Reference Manual, <https://docs.mimer.com>)
const MIMER: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("REFERENCES", &[T]),
    ("ALL", &[T, V]),
    ("EXECUTE", &[P, F]),
];

/// Views are read-only; COPY FROM / COPY INTO are global.
/// <https://www.monetdb.org/documentation/user-guide/sql-manual/data-definition/privileges/>
const MONETDB: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T]),
    ("UPDATE", &[T]),
    ("DELETE", &[T]),
    ("TRUNCATE", &[T]),
    ("REFERENCES", &[T]),
    ("ALL", &[T]),
    ("EXECUTE", &[P, F]),
];

/// GRANT (privilege) on tables, views and procedures; the database
/// privileges (`ON DATABASE`) aren't offered.
/// <https://docs.actian.com/ingres/11.2/index.html#page/SQLRef/GRANT_(privilege).htm>
const INGRES: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("REFERENCES", &[T]),
    ("COPY_INTO", &[T]),
    ("COPY_FROM", &[T]),
    ("ALL", &[T, V]),
    ("EXECUTE", &[P]),
];

/// Object privileges (on a schema they cover its objects) and the
/// administrative privileges. <https://docs.intersystems.com/irislatest/csp/docbook/DocBook.UI.Page.cls?KEY=RSQL_grant>
const IRIS: Objects = &[
    ("SELECT", &[SC, T, V]),
    ("INSERT", &[SC, T, V]),
    ("UPDATE", &[SC, T, V]),
    ("DELETE", &[SC, T, V]),
    ("REFERENCES", &[SC, T]),
    ("ALTER", &[SC, T, V]),
    ("EXECUTE", &[SC, P]),
    ("ALL", &[SC, T, V]),
];

const IRIS_ADMIN: &[&str] = &[
    "%CREATE_TABLE",
    "%ALTER_TABLE",
    "%DROP_TABLE",
    "%CREATE_VIEW",
    "%ALTER_VIEW",
    "%DROP_VIEW",
    "%CREATE_PROCEDURE",
    "%DROP_PROCEDURE",
    "%CREATE_FUNCTION",
    "%DROP_FUNCTION",
    "%CREATE_METHOD",
    "%DROP_METHOD",
    "%CREATE_QUERY",
    "%DROP_QUERY",
    "%CREATE_TRIGGER",
    "%DROP_TRIGGER",
    "%NOCHECK",
    "%NOINDEX",
    "%NOLOCK",
    "%NOTRIGGER",
];

/// SAP MaxDB SQL Reference Manual, GRANT statement: table, view,
/// procedure and schema privileges. <https://maxdb.sap.com/documentation/>
const MAXDB: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("SELUPD", &[T, V]),
    ("ALTER", &[T]),
    ("INDEX", &[T]),
    ("REFERENCES", &[T]),
    ("ALL", &[T, V]),
    ("EXECUTE", &[P]),
    ("CREATEIN", &[SC]),
    ("DROPIN", &[SC]),
    ("ALTERIN", &[SC]),
];

/// <https://doc.nuodb.com/nuodb/latest/reference-information/sql-language/sql-statements/grant/>
const NUODB: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T]),
    ("UPDATE", &[T]),
    ("DELETE", &[T]),
    ("ALTER", &[SC, T, V, P, F]),
    ("GRANT", &[T, V]),
    ("TRIGGER", &[T]),
    ("ALL", &[T, V]),
    ("CREATE", &[SC]),
    ("EXECUTE", &[P, F]),
];

/// Table and view privileges; the database ones (`ON DATABASE`) aren't
/// offered. <https://docs.heavy.ai/sql/data-definition-ddl/users-and-databases>
const HEAVYDB: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T]),
    ("UPDATE", &[T]),
    ("DELETE", &[T]),
    ("TRUNCATE", &[T]),
    ("ALTER", &[T]),
    ("DROP", &[T, V]),
    ("ALL", &[T, V]),
];

/// Cluster (no object), schema, table and view permissions; the database
/// ones (`ON DATABASE`) aren't offered.
/// <https://docs.sqream.com/en/latest/reference/sql/sql_statements/access_control_commands/grant.html>
const SQREAM: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T]),
    ("UPDATE", &[T]),
    ("DELETE", &[T]),
    ("DDL", &[SC, T, V]),
    ("ALL", &[SC, T, V]),
    ("USAGE", &[SC]),
    ("CREATE", &[SC]),
    ("SUPERUSER", &[S, SC]),
];

/// <https://docs.openlinksw.com/virtuoso/grant/>
const VIRTUOSO: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("REFERENCES", &[T]),
    ("ALL", &[T, V]),
    ("EXECUTE", &[P]),
];

/// The database-wide privileges (DBA, RESOURCE, the audit ones) and the
/// table and view ones. <https://docs.progress.com/bundle/openedge-sql-reference/page/GRANT.html>
const OPENEDGE: Objects = &[
    ("SELECT", &[T, V]),
    ("INSERT", &[T, V]),
    ("UPDATE", &[T, V]),
    ("DELETE", &[T, V]),
    ("INDEX", &[T]),
    ("REFERENCES", &[T]),
    ("ALTER", &[T]),
    ("ALL", &[T, V]),
];

/// Machbase DBMS SQL reference, user management (GRANT on tables).
/// <https://docs.machbase.com/dbms/>
const MACHBASE: Objects = &[
    ("SELECT", &[T]),
    ("INSERT", &[T]),
    ("UPDATE", &[T]),
    ("DELETE", &[T]),
    ("ALL", &[T]),
];

/// Ocient's system ("" = `ON SYSTEM`), schema, table and view privileges;
/// the database ones (`ON DATABASE`) aren't offered.
/// <https://docs.ocient.com/data-control-language-dcl-statement-reference>
const OCIENT: Objects = &[
    ("SELECT", &[S, SC, T, V]),
    ("INSERT", &[SC, T]),
    ("DELETE", &[S, SC, T]),
    ("LOAD", &[T]),
    ("ALTER", &[SC, T, V]),
    ("DROP", &[SC, T, V]),
    ("VIEW", &[SC, T, V]),
    ("SYSAUTH", &[S, SC, T, V]),
    ("ALL", &[S, SC, T, V]),
    ("CREATE TABLE", &[S, SC]),
    ("CREATE VIEW", &[S, SC]),
    ("CREATE MLMODEL", &[SC]),
    ("VIEW TABLE", &[S, SC]),
    ("VIEW VIEW", &[S, SC]),
    ("ALTER TABLE", &[S, SC]),
    ("ALTER VIEW", &[S, SC]),
    ("DROP TABLE", &[S, SC]),
    ("DROP VIEW", &[S, SC]),
];

const OCIENT_SYSTEM: &[&str] = &[
    "CREATE DATABASE",
    "CREATE USER",
    "CREATE GROUP",
    "VIEW SCHEMA",
    "ALTER SCHEMA",
    "DROP SCHEMA",
    "VIEW QUERIES",
    "VIEW REDACTED QUERIES",
    "CANCEL QUERIES",
    "USE",
];

#[cfg(test)]
mod tests {
    use super::super::{script, spec};
    use super::*;
    use dbine_driver::{ObjectRef, SecurityAction};

    const DIALECTS: [Dialect; 29] = [
        Dialect::Hive,
        Dialect::Impala,
        Dialect::Vertica,
        Dialect::Exasol,
        Dialect::Db2,
        Dialect::Teradata,
        Dialect::Informix,
        Dialect::Db2i,
        Dialect::Db2z,
        Dialect::Ase,
        Dialect::SqlAnywhere,
        Dialect::Netezza,
        Dialect::Altibase,
        Dialect::Dameng,
        Dialect::Cubrid,
        Dialect::Zen,
        Dialect::Mimer,
        Dialect::MonetDb,
        Dialect::Ingres,
        Dialect::Iris,
        Dialect::MaxDb,
        Dialect::NuoDb,
        Dialect::HeavyDb,
        Dialect::Sqream,
        Dialect::Virtuoso,
        Dialect::OpenEdge,
        Dialect::Machbase,
        Dialect::Ignite,
        Dialect::Ocient,
    ];

    fn target(kind: &str) -> Option<ObjectRef> {
        match kind {
            "" => None,
            "schema" => Some(ObjectRef {
                kind: kind.into(),
                schema: None,
                name: "ventas".into(),
            }),
            _ => Some(ObjectRef {
                kind: kind.into(),
                schema: Some("ventas".into()),
                name: "facturas".into(),
            }),
        }
    }

    /// Every privilege goes only on the spec's kinds, once, and the
    /// driver's own script takes it on each of them.
    #[test]
    fn every_privilege_is_grantable_on_its_kinds() {
        for d in DIALECTS {
            let kinds = spec(d).object_kinds;
            let list = grantable(d);
            assert_eq!(list.is_empty(), d == Dialect::Ignite, "{d:?}");
            let mut names = std::collections::HashSet::new();
            for p in &list {
                assert!(names.insert(p.name.as_str()), "{d:?}: {} twice", p.name);
                assert!(!p.on.is_empty(), "{d:?}: {} on nothing", p.name);
                for k in &p.on {
                    assert!(kinds.contains(&k.as_str()), "{d:?}: {} on «{k}»", p.name);
                    let a = SecurityAction::Grant {
                        privileges: vec![p.name.clone()],
                        object: target(k),
                        to: "ana".into(),
                        grantable: false,
                    };
                    let sql =
                        script(d, &a).unwrap_or_else(|e| panic!("{d:?}: {} on «{k}»: {e}", p.name));
                    assert!(
                        sql.to_uppercase().contains(&p.name.to_uppercase()),
                        "{d:?}: {sql}"
                    );
                }
            }
        }
    }

    #[test]
    fn privileges_go_where_the_engine_takes_them() {
        let on = |d: Dialect, n: &str| {
            grantable(d)
                .into_iter()
                .find(|p| p.name == n)
                .map(|p| p.on.join(","))
                .unwrap_or_else(|| panic!("{d:?}: {n}"))
        };
        assert_eq!(on(Dialect::Db2, "CONNECT"), "");
        assert_eq!(on(Dialect::Db2, "CREATEIN"), "schema");
        assert_eq!(on(Dialect::Db2, "INDEX"), "table");
        assert_eq!(on(Dialect::Informix, "EXECUTE"), "procedure,function");
        assert_eq!(
            on(Dialect::Teradata, "EXECUTE PROCEDURE"),
            "schema,procedure"
        );
        assert_eq!(on(Dialect::Exasol, "CREATE SESSION"), "");
        assert_eq!(on(Dialect::Impala, "CREATE"), ",schema");
        assert_eq!(on(Dialect::Ocient, "SELECT"), ",schema,table,view");
        assert_eq!(on(Dialect::Iris, "%CREATE_TABLE"), "");
        assert_eq!(on(Dialect::Netezza, "ALL ADMIN"), "");
    }
}

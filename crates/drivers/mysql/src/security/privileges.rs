//! What each engine can grant (`Session::privileges`), on DBine's object
//! kinds (`SecuritySpec::object_kinds`).
//!
//! MySQL, MariaDB, TiDB, OceanBase and Doris answer `SHOW PRIVILEGES`
//! (privilege, context, comment), so the list follows the server's version
//! (MySQL's dynamic privileges, MariaDB's per-version ones…). Where it
//! can't be read, and for StarRocks, SingleStore and Databend, which don't
//! have it, each engine's documented privileges per kind.

use super::{script_for, spec_for};
use crate::session::{at, MySqlSession};
use crate::Variant;
use dbine_driver::{Privilege, SecurityAction};

// Kinds, with the wider levels a grant there implies: a table privilege
// can also be granted on its database (`db.*`) and on everything (`*.*`).
const SERVER: &[&str] = &[""];
const DATABASES: &[&str] = &["", "schema"];
const TABLES: &[&str] = &["", "schema", "table", "view"];
const ROUTINES: &[&str] = &["", "schema", "procedure", "function"];

/// MySQL's static privileges, which MariaDB, TiDB and OceanBase share:
/// "Permissible Static Privileges for GRANT and REVOKE" in
/// https://dev.mysql.com/doc/refman/8.4/en/grant.html (levels in
/// https://dev.mysql.com/doc/refman/8.4/en/privileges-provided.html).
/// Left out: GRANT OPTION (the "con opción de otorgar" switch), USAGE (no
/// privilege) and PROXY (granted on an account, not an object).
const MYSQL_STATIC: &[(&str, &[&str])] = &[
    ("SELECT", TABLES),
    ("INSERT", TABLES),
    ("UPDATE", TABLES),
    ("DELETE", TABLES),
    ("CREATE", TABLES),
    ("ALTER", TABLES),
    ("DROP", TABLES),
    ("INDEX", TABLES),
    ("REFERENCES", TABLES),
    ("CREATE VIEW", TABLES),
    ("SHOW VIEW", TABLES),
    ("TRIGGER", TABLES),
    ("EXECUTE", ROUTINES),
    ("ALTER ROUTINE", ROUTINES),
    ("CREATE ROUTINE", DATABASES),
    ("CREATE TEMPORARY TABLES", DATABASES),
    ("LOCK TABLES", DATABASES),
    ("EVENT", DATABASES),
    ("CREATE USER", SERVER),
    ("CREATE TABLESPACE", SERVER),
    ("FILE", SERVER),
    ("PROCESS", SERVER),
    ("RELOAD", SERVER),
    ("REPLICATION CLIENT", SERVER),
    ("REPLICATION SLAVE", SERVER),
    ("SHOW DATABASES", SERVER),
    ("SHUTDOWN", SERVER),
    ("SUPER", SERVER),
];

/// MySQL 8's server-wide extras: CREATE ROLE, DROP ROLE and the dynamic
/// privileges the server itself registers (plugins add theirs to SHOW
/// PRIVILEGES), as of 8.0.30:
/// https://dev.mysql.com/doc/refman/8.0/en/privileges-provided.html#privileges-provided-dynamic
const MYSQL_SERVER: &[&str] = &[
    "CREATE ROLE",
    "DROP ROLE",
    "APPLICATION_PASSWORD_ADMIN",
    "AUDIT_ABORT_EXEMPT",
    "AUDIT_ADMIN",
    "AUTHENTICATION_POLICY_ADMIN",
    "BACKUP_ADMIN",
    "BINLOG_ADMIN",
    "BINLOG_ENCRYPTION_ADMIN",
    "CLONE_ADMIN",
    "CONNECTION_ADMIN",
    "ENCRYPTION_KEY_ADMIN",
    "FLUSH_OPTIMIZER_COSTS",
    "FLUSH_STATUS",
    "FLUSH_TABLES",
    "FLUSH_USER_RESOURCES",
    "GROUP_REPLICATION_ADMIN",
    "GROUP_REPLICATION_STREAM",
    "INNODB_REDO_LOG_ARCHIVE",
    "INNODB_REDO_LOG_ENABLE",
    "PASSWORDLESS_USER_ADMIN",
    "PERSIST_RO_VARIABLES_ADMIN",
    "REPLICATION_APPLIER",
    "REPLICATION_SLAVE_ADMIN",
    "RESOURCE_GROUP_ADMIN",
    "RESOURCE_GROUP_USER",
    "ROLE_ADMIN",
    "SENSITIVE_VARIABLES_OBSERVER",
    "SERVICE_CONNECTION_ADMIN",
    "SESSION_VARIABLES_ADMIN",
    "SHOW_ROUTINE",
    "SYSTEM_USER",
    "SYSTEM_VARIABLES_ADMIN",
    "TABLE_ENCRYPTION_ADMIN",
    "XA_RECOVER_ADMIN",
];

/// MariaDB's own (https://mariadb.com/kb/en/grant/#privilege-levels, as
/// of 11.4): DELETE HISTORY on tables, SHOW CREATE ROUTINE on routines and
/// the server privileges that split SUPER.
const MARIADB: &[(&str, &[&str])] = &[
    ("DELETE HISTORY", TABLES),
    ("SHOW CREATE ROUTINE", ROUTINES),
    ("BINLOG ADMIN", SERVER),
    ("BINLOG MONITOR", SERVER),
    ("BINLOG REPLAY", SERVER),
    ("CONNECTION ADMIN", SERVER),
    ("FEDERATED ADMIN", SERVER),
    ("READ_ONLY ADMIN", SERVER),
    ("REPLICATION MASTER ADMIN", SERVER),
    ("REPLICATION SLAVE ADMIN", SERVER),
    ("SET USER", SERVER),
    ("SLAVE MONITOR", SERVER),
];

/// TiDB's server-wide extras and dynamic privileges:
/// https://docs.pingcap.com/tidb/stable/privilege-management
const TIDB_SERVER: &[&str] = &[
    "CONFIG",
    "CREATE ROLE",
    "DROP ROLE",
    "BACKUP_ADMIN",
    "RESTORE_ADMIN",
    "SYSTEM_USER",
    "SYSTEM_VARIABLES_ADMIN",
    "ROLE_ADMIN",
    "CONNECTION_ADMIN",
    "PLACEMENT_ADMIN",
    "DASHBOARD_CLIENT",
    "RESTRICTED_TABLES_ADMIN",
    "RESTRICTED_STATUS_ADMIN",
    "RESTRICTED_VARIABLES_ADMIN",
    "RESTRICTED_USER_ADMIN",
    "RESTRICTED_CONNECTION_ADMIN",
    "RESTRICTED_REPLICA_WRITER_ADMIN",
    "RESOURCE_GROUP_ADMIN",
];

/// Doris (and VeloDB):
/// https://doris.apache.org/docs/admin-manual/auth/authentication-and-authorization
/// on the levels DBine offers (`*.*.*` and a database). USAGE_PRIV goes
/// only on resources and workload groups, which it doesn't.
const DORIS: &[(&str, &[&str])] = &[
    ("SELECT_PRIV", DATABASES),
    ("LOAD_PRIV", DATABASES),
    ("ALTER_PRIV", DATABASES),
    ("CREATE_PRIV", DATABASES),
    ("DROP_PRIV", DATABASES),
    ("SHOW_VIEW_PRIV", DATABASES),
    ("GRANT_PRIV", DATABASES),
    ("ADMIN_PRIV", SERVER),
    ("NODE_PRIV", SERVER),
];

/// StarRocks 3.x and 4.x, per object type
/// (https://docs.starrocks.io/docs/administration/user_privs/privilege_item/;
/// its SHOW PRIVILEGES answers nothing): "" is SYSTEM, plus the default
/// catalog's CREATE DATABASE and USAGE; a database takes its own
/// privileges (ALTER, DROP, CREATE TABLE…) and its tables' (SELECT,
/// INSERT…, granted on ALL TABLES IN DATABASE). NODE and GRANT go only
/// through the built-in cluster_admin and user_admin roles: StarRocks
/// refuses them to a user or role.
const STARROCKS: &[(&str, &[&str])] = &[
    ("SELECT", &["schema", "table", "view", "materialized_view"]),
    ("INSERT", &["schema", "table"]),
    ("UPDATE", &["schema", "table"]),
    ("DELETE", &["schema", "table"]),
    ("EXPORT", &["schema", "table"]),
    ("ALTER", &["schema", "table", "view", "materialized_view"]),
    ("DROP", &["schema", "table", "view", "materialized_view"]),
    ("REFRESH", &["materialized_view"]),
    ("ALL", &["schema", "table", "view", "materialized_view"]),
    ("CREATE TABLE", &["schema"]),
    ("CREATE VIEW", &["schema"]),
    ("CREATE MATERIALIZED VIEW", &["schema"]),
    ("CREATE FUNCTION", &["schema"]),
    ("CREATE PIPE", &["schema"]),
    ("CREATE DATABASE", SERVER),
    ("USAGE", SERVER),
    ("OPERATE", SERVER),
    ("SECURITY", SERVER),
    ("CREATE WAREHOUSE", SERVER),
    ("CREATE RESOURCE GROUP", SERVER),
    ("CREATE RESOURCE", SERVER),
    ("CREATE EXTERNAL CATALOG", SERVER),
    ("CREATE GLOBAL FUNCTION", SERVER),
    ("CREATE STORAGE VOLUME", SERVER),
    ("PLUGIN", SERVER),
    ("FILE", SERVER),
    ("REPOSITORY", SERVER),
    ("BLACKLIST", SERVER),
];

/// SingleStore's "Allowed Scopes in Default Mode"
/// (https://docs.singlestore.com/cloud/reference/sql-reference/security-management-commands/permissions-matrix/)
/// on the kinds DBine offers; a grant on `db.name` doesn't tell a table
/// from a view. Left out: GRANT OPTION, USAGE (resource pools) and CREATE
/// / DROP POOL (Helios only).
const SINGLESTORE: &[(&str, &[&str])] = &[
    ("SELECT", TABLES),
    ("INSERT", TABLES),
    ("UPDATE", TABLES),
    ("DELETE", TABLES),
    ("CREATE", TABLES),
    ("DROP", TABLES),
    ("INDEX", TABLES),
    ("ALTER", TABLES),
    ("CREATE PIPELINE", TABLES),
    ("DROP PIPELINE", TABLES),
    ("ALTER PIPELINE", TABLES),
    ("START PIPELINE", TABLES),
    ("SHOW PIPELINE", TABLES),
    ("DROP VIEW", TABLES),
    ("CREATE VIEW", &["", "schema", "view"]),
    ("SHOW VIEW", &["", "schema", "view"]),
    ("ALTER VIEW", &["", "schema", "view"]),
    ("CREATE ROUTINE", DATABASES),
    ("ALTER ROUTINE", DATABASES),
    ("EXECUTE", DATABASES),
    ("RELOAD", DATABASES),
    ("CREATE TEMPORARY TABLES", DATABASES),
    ("LOCK TABLES", DATABASES),
    ("BACKUP", DATABASES),
    ("CREATE DATABASE", DATABASES),
    ("DROP DATABASE", DATABASES),
    ("CREATE LINK", DATABASES),
    ("DROP LINK", DATABASES),
    ("SHOW LINK", DATABASES),
    ("SHOW ROUTINE", &["schema"]),
    ("PROCESS", SERVER),
    ("SHOW METADATA", SERVER),
    ("SYSTEM_VARIABLES_ADMIN", SERVER),
    ("CREATE USER", SERVER),
    ("ALTER USER", SERVER),
    ("OUTBOUND", SERVER),
    ("ALTER EVENT TRACE", SERVER),
    ("CLUSTER", SERVER),
    ("FILE READ", SERVER),
    ("FILE WRITE", SERVER),
    ("ALL PRIVILEGES", TABLES),
];

/// Databend (https://docs.databend.com/guides/security/access-control/privileges):
/// table privileges also on a database and on everything; the rest only
/// on everything (`*.*`, where stage privileges cover every stage);
/// OWNERSHIP only on an object.
const DATABEND: &[(&str, &[&str])] = &[
    ("SELECT", TABLES),
    ("INSERT", TABLES),
    ("UPDATE", TABLES),
    ("DELETE", TABLES),
    ("ALTER", TABLES),
    ("DROP", TABLES),
    ("CREATE", TABLES),
    ("SUPER", TABLES),
    ("ALL", TABLES),
    ("OWNERSHIP", &["schema", "table", "view"]),
    ("ATTACH TABLE", DATABASES),
    ("CREATE DATABASE", SERVER),
    ("CREATE USER", SERVER),
    ("DROP USER", SERVER),
    ("CREATE ROLE", SERVER),
    ("DROP ROLE", SERVER),
    ("GRANT", SERVER),
    ("USAGE", SERVER),
    ("READ", SERVER),
    ("WRITE", SERVER),
    ("CREATE WAREHOUSE", SERVER),
    ("CREATE CONNECTION", SERVER),
    ("ACCESS CONNECTION", SERVER),
    ("CREATE SEQUENCE", SERVER),
    ("ACCESS SEQUENCE", SERVER),
    ("CREATE PROCEDURE", SERVER),
    ("ACCESS PROCEDURE", SERVER),
    ("CREATE MASKING POLICY", SERVER),
    ("APPLY MASKING POLICY", SERVER),
    ("CREATE ROW ACCESS POLICY", SERVER),
    ("APPLY ROW ACCESS POLICY", SERVER),
];

/// The engines with `SHOW PRIVILEGES`.
fn has_catalog(v: Variant) -> bool {
    matches!(
        v,
        Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase | Variant::Doris
    )
}

/// MySQL's family offers ALL PRIVILEGES on every kind.
fn mysql_family(v: Variant) -> bool {
    matches!(
        v,
        Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase
    )
}

/// The kinds a `SHOW PRIVILEGES` context names ("Databases,Tables,Indexes",
/// "Functions,Procedures", "Server Admin"; Doris': "GLOBAL,CATALOG,
/// DATABASE,TABLE"), with the wider levels they imply. Contexts DBine has
/// no kind for (Indexes, CATALOG, RESOURCE…) add nothing.
fn context_kinds(context: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    for c in context.split(',') {
        out.extend_from_slice(match c.trim().to_ascii_lowercase().as_str() {
            "server admin" | "server" | "file access on server" | "global" => SERVER,
            "databases" | "database" => DATABASES,
            "tables" | "table" => TABLES,
            "functions" => &["", "schema", "function"],
            "procedures" => &["", "schema", "procedure"],
            _ => &[],
        });
    }
    out
}

/// `SHOW PRIVILEGES` rows (privilege, context) as (NAME, kinds), upper-cased
/// as GRANT writes them. GRANT OPTION, USAGE and PROXY aren't privileges
/// to pick (a switch, nothing, an account); EVENT says "Server Admin" but
/// is granted on a database too.
fn from_catalog(rows: &[(String, String)]) -> Vec<(String, Vec<&'static str>)> {
    rows.iter()
        .map(|(name, context)| {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            let name = name.to_uppercase();
            let kinds = if name == "EVENT" {
                DATABASES.to_vec()
            } else {
                context_kinds(context)
            };
            (name, kinds)
        })
        .filter(|(name, _)| !matches!(name.as_str(), "GRANT OPTION" | "USAGE" | "PROXY"))
        .collect()
}

/// The documented privileges of an engine, as (NAME, kinds).
fn documented(v: Variant) -> Vec<(String, Vec<&'static str>)> {
    let table = |t: &[(&str, &[&'static str])]| -> Vec<(String, Vec<&'static str>)> {
        t.iter().map(|(n, k)| (n.to_string(), k.to_vec())).collect()
    };
    let server = |t: &[&str]| -> Vec<(String, Vec<&'static str>)> {
        t.iter().map(|n| (n.to_string(), SERVER.to_vec())).collect()
    };
    match v {
        Variant::StarRocks => table(STARROCKS),
        Variant::Doris => table(DORIS),
        Variant::SingleStore => table(SINGLESTORE),
        Variant::Databend => table(DATABEND),
        Variant::MariaDb => [table(MYSQL_STATIC), table(MARIADB)].concat(),
        Variant::TiDb => [table(MYSQL_STATIC), server(TIDB_SERVER)].concat(),
        Variant::MySql => [table(MYSQL_STATIC), server(MYSQL_SERVER)].concat(),
        _ => table(MYSQL_STATIC),
    }
}

/// The list the tab offers: each privilege on the kinds the engine's spec
/// has (in its order), without the ones left with none, repeated or that
/// the engine's GRANT script would refuse; MySQL's family adds ALL
/// PRIVILEGES on every kind. TiDB has no stored routines.
fn finish(v: Variant, list: Vec<(String, Vec<&'static str>)>) -> Vec<Privilege> {
    let mut kinds = spec_for(v).object_kinds;
    if v == Variant::TiDb {
        kinds.retain(|k| *k != "procedure" && *k != "function");
    }
    let mut list = list;
    if mysql_family(v) {
        list.push(("ALL PRIVILEGES".into(), kinds.clone()));
    }
    let mut out: Vec<Privilege> = Vec::new();
    for (name, on) in list {
        let on: Vec<&str> = kinds.iter().copied().filter(|k| on.contains(k)).collect();
        let scripted = script_for(
            v,
            &SecurityAction::Grant {
                privileges: vec![name.clone()],
                object: None,
                to: "r".into(),
                grantable: false,
            },
        )
        .is_ok();
        if on.is_empty() || !scripted || out.iter().any(|p| p.name == name) {
            continue;
        }
        out.push(Privilege::new(name, &on));
    }
    out
}

/// What the server can grant: its `SHOW PRIVILEGES` where it has one, the
/// documented list where it doesn't or the command fails.
pub async fn privileges(s: &mut MySqlSession) -> Vec<Privilege> {
    let v = s.variant;
    if has_catalog(v) {
        let rows: Vec<(String, String)> = s
            .optional_rows("SHOW PRIVILEGES")
            .await
            .iter()
            .filter_map(|r| Some((at(r, 0)?, at(r, 1).unwrap_or_default())))
            .collect();
        let listed = finish(v, from_catalog(&rows));
        // ALL PRIVILEGES alone: the catalog said nothing.
        if listed.iter().any(|p| p.name != "ALL PRIVILEGES") {
            return listed;
        }
    }
    finish(v, documented(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn rows(r: &[(&str, &str)]) -> Vec<(String, String)> {
        r.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn on<'a>(p: &'a [Privilege], name: &str) -> Option<&'a [String]> {
        p.iter().find(|p| p.name == name).map(|p| p.on.as_slice())
    }

    #[test]
    fn maps_show_privileges() {
        // MySQL 8.4 / MariaDB 11 / TiDB rows.
        let catalog = rows(&[
            ("Alter", "Tables"),
            ("Alter routine", "Functions,Procedures"),
            ("Create", "Databases,Tables,Indexes"),
            ("Create routine", "Databases"),
            ("Event", "Server Admin"),
            ("File", "File access on server"),
            ("Grant option", "Databases,Tables,Functions,Procedures"),
            ("Proxy", "Server Admin"),
            ("Usage", "Server Admin"),
            ("Show Create Routine", "Databases,Functions,Procedures"),
            ("Read_only admin", "Server"),
            ("BACKUP_ADMIN", "Server Admin"),
            ("SOMETHING", "Indexes"),
        ]);
        let p = finish(Variant::MySql, from_catalog(&catalog));
        assert_eq!(on(&p, "ALTER").unwrap(), ["", "schema", "table", "view"]);
        assert_eq!(
            on(&p, "ALTER ROUTINE").unwrap(),
            ["", "schema", "procedure", "function"]
        );
        assert_eq!(on(&p, "CREATE").unwrap(), ["", "schema", "table", "view"]);
        assert_eq!(on(&p, "CREATE ROUTINE").unwrap(), ["", "schema"]);
        assert_eq!(on(&p, "EVENT").unwrap(), ["", "schema"]);
        assert_eq!(on(&p, "FILE").unwrap(), [""]);
        assert_eq!(
            on(&p, "SHOW CREATE ROUTINE").unwrap(),
            ["", "schema", "procedure", "function"]
        );
        assert_eq!(on(&p, "READ_ONLY ADMIN").unwrap(), [""]);
        assert_eq!(on(&p, "BACKUP_ADMIN").unwrap(), [""]);
        for gone in ["GRANT OPTION", "PROXY", "USAGE", "SOMETHING"] {
            assert!(on(&p, gone).is_none(), "{gone}");
        }
        assert_eq!(
            on(&p, "ALL PRIVILEGES").unwrap(),
            ["", "schema", "table", "view", "procedure", "function"]
        );
        // TiDB: no routines.
        let p = finish(Variant::TiDb, from_catalog(&catalog));
        assert_eq!(on(&p, "ALTER ROUTINE").unwrap(), ["", "schema"]);
        assert_eq!(
            on(&p, "ALL PRIVILEGES").unwrap(),
            ["", "schema", "table", "view"]
        );
        // Doris: GLOBAL / DATABASE, a table not offered; resource-only out.
        let doris = rows(&[
            ("Node_priv", "GLOBAL"),
            ("Select_priv", "GLOBAL,CATALOG,DATABASE,TABLE"),
            ("Usage_priv", "RESOURCE,WORKLOAD GROUP"),
        ]);
        let p = finish(Variant::Doris, from_catalog(&doris));
        assert_eq!(on(&p, "NODE_PRIV").unwrap(), [""]);
        assert_eq!(on(&p, "SELECT_PRIV").unwrap(), ["", "schema"]);
        assert!(on(&p, "USAGE_PRIV").is_none() && on(&p, "ALL PRIVILEGES").is_none());
    }

    /// Every documented privilege, granted on each kind it's listed for,
    /// gets a script.
    #[test]
    fn every_documented_privilege_has_a_script() {
        for v in [
            Variant::MySql,
            Variant::MariaDb,
            Variant::TiDb,
            Variant::OceanBase,
            Variant::StarRocks,
            Variant::Doris,
            Variant::SingleStore,
            Variant::Databend,
        ] {
            let kinds = spec_for(v).object_kinds;
            let all = finish(v, documented(v));
            assert_eq!(
                all.len(),
                documented(v).len() + usize::from(mysql_family(v)),
                "{v:?}: a documented privilege was dropped"
            );
            for p in all {
                assert!(!p.on.is_empty(), "{v:?} {}", p.name);
                for k in &p.on {
                    assert!(kinds.contains(&k.as_str()), "{v:?} {} on {k}", p.name);
                    let object = match k.as_str() {
                        "" => None,
                        "schema" => Some(ObjectRef {
                            kind: k.clone(),
                            schema: None,
                            name: "d".into(),
                        }),
                        _ => Some(ObjectRef {
                            kind: k.clone(),
                            schema: Some("d".into()),
                            name: "t".into(),
                        }),
                    };
                    let to = match v {
                        Variant::StarRocks | Variant::Doris | Variant::Databend => "role:r",
                        _ => "r",
                    };
                    script_for(
                        v,
                        &SecurityAction::Grant {
                            privileges: vec![p.name.clone()],
                            object,
                            to: to.into(),
                            grantable: false,
                        },
                    )
                    .unwrap_or_else(|e| panic!("{v:?} {} on {k}: {e}", p.name));
                }
            }
        }
    }
}

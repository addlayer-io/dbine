//! What the login may do (`Session::permissions`), in one query per
//! variant. Only the actions the variant offers are filled (see `offered`);
//! the rest stay unknown.
//!
//! - PostgreSQL and the engines that keep its catalog (managed services,
//!   EDB, Fujitsu, KingbaseES, TimescaleDB, YugabyteDB, Greenplum and its
//!   forks, Yellowbrick): `pg_roles` (`rolsuper`, `rolcreatedb`,
//!   `rolcreaterole`), membership in `pg_read_all_stats` (the profiler reads
//!   other users' statements in `pg_stat_activity`; `pg_monitor` includes
//!   it) and `pg_signal_backend` (`pg_terminate_backend` on other users'
//!   backends), and ownership of the database (DROP DATABASE). Before
//!   PostgreSQL 10 / 9.6 those roles don't exist and only a superuser may.
//!   Yellowbrick's profiler and kill are its own (`sys.log_query`,
//!   `yb_terminate_session`): unknown unless superuser.
//! - openGauss: `rolsystemadmin` counts as superuser and `rolmonitoradmin`
//!   sees other sessions.
//! - CockroachDB: the `admin` role, the CREATEDB / CREATEROLE options
//!   (`pg_roles`), system privileges (`has_system_privilege`) and the
//!   database's grants (`SHOW GRANTS ON DATABASE`). The VIEWACTIVITY and
//!   CANCELQUERY role options aren't readable without admin: without the
//!   system privilege of the same name, profiler and kill stay unknown.
//! - Redshift and RisingWave: `pg_user` (`usesuper`, `usecreatedb`); the
//!   rest (Redshift's RBAC system privileges) stays unknown.
//! - Materialize: `mz_is_superuser()` and the CREATEDB / CREATEROLE system
//!   privileges.
//! - CrateDB: superuser, or the AL privilege on the cluster granted to the
//!   user itself (one granted through a role stays unknown).
//! - H2: the ADMIN right, which every one of its offered actions needs.
//! - Denodo: nothing (it manages its users in its own server).
//!
//! Creating a schema needs CREATE on the database
//! (`has_database_privilege`, which counts the owner); the query that reads
//! it is tried first and, if the server refuses it, the one without it.
//! RisingWave doesn't say and H2 grants it with ALTER ANY SCHEMA, which
//! isn't read: unknown unless administrator.
//!
//! A check the server refuses leaves everything unknown; only a broken
//! connection is an error.

use crate::catalog::{cell, lit};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Access, Error, Permissions, Result};
use std::time::Duration;
use tokio_postgres::SimpleQueryRow;

/// Longest the check may take (it must never hang the explorer).
const LIMIT: Duration = Duration::from_secs(5);

/// `{db}`: the database's name as a literal.
const PG_SQL: &str = "SELECT r.rolsuper::text AS super, r.rolcreatedb::text AS createdb, r.rolcreaterole::text AS createrole,
       (CASE WHEN EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pg_read_all_stats')
             THEN pg_has_role('pg_read_all_stats', 'MEMBER') ELSE false END)::text AS stats,
       (CASE WHEN EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pg_signal_backend')
             THEN pg_has_role('pg_signal_backend', 'MEMBER') ELSE false END)::text AS signal,
       (SELECT pg_has_role(d.datdba, 'USAGE') FROM pg_database d WHERE d.datname = {db})::text AS owner
  FROM pg_roles r WHERE r.rolname = current_user";

/// openGauss: SYSADMIN and MONADMIN instead of the predefined roles.
const OPENGAUSS_SQL: &str = "SELECT (r.rolsuper OR r.rolsystemadmin)::text AS super, r.rolcreatedb::text AS createdb,
       r.rolcreaterole::text AS createrole, r.rolmonitoradmin::text AS stats,
       (SELECT pg_has_role(d.datdba, 'USAGE') FROM pg_database d WHERE d.datname = {db})::text AS owner
  FROM pg_roles r WHERE r.rolname = current_user";

/// CockroachDB. `{db}`: the literal, `{ident}`: the quoted identifier.
const COCKROACH_SQL: &str = "SELECT pg_has_role('admin', 'MEMBER')::text AS super,
       r.rolcreatedb::text AS createdb, r.rolcreaterole::text AS createrole,
       has_system_privilege('CREATEDB')::text AS sys_createdb, has_system_privilege('CREATEROLE')::text AS sys_createrole,
       (has_system_privilege('VIEWACTIVITY') OR has_system_privilege('VIEWACTIVITYREDACTED'))::text AS stats,
       has_system_privilege('CANCELQUERY')::text AS signal,
       has_system_privilege('BACKUP')::text AS sys_backup, has_system_privilege('RESTORE')::text AS sys_restore,
       EXISTS (SELECT 1 FROM [SHOW GRANTS ON DATABASE {ident}] g
                WHERE g.privilege_type IN ('BACKUP', 'ALL') AND pg_has_role(g.grantee, 'MEMBER'))::text AS db_backup,
       EXISTS (SELECT 1 FROM [SHOW GRANTS ON DATABASE {ident}] g
                WHERE g.privilege_type IN ('DROP', 'ALL') AND pg_has_role(g.grantee, 'MEMBER'))::text AS db_drop,
       (SELECT pg_has_role(d.datdba, 'USAGE') FROM pg_database d WHERE d.datname = {db})::text AS owner
  FROM pg_roles r WHERE r.rolname = current_user";

/// CockroachDB before system privileges (22.2): role options only.
const COCKROACH_OLD_SQL: &str = "SELECT pg_has_role('admin', 'MEMBER')::text AS super,
       r.rolcreatedb::text AS createdb, r.rolcreaterole::text AS createrole,
       (SELECT pg_has_role(d.datdba, 'USAGE') FROM pg_database d WHERE d.datname = {db})::text AS owner
  FROM pg_roles r WHERE r.rolname = current_user";

/// Redshift, with CREATE on the database (`{db}`).
const REDSHIFT_SQL: &str = "SELECT usesuper::text AS super, usecreatedb::text AS createdb,
       has_database_privilege({db}, 'CREATE')::text AS db_create
  FROM pg_user WHERE usename = current_user";

/// Materialize, with CREATE on the database (`{db}`).
const MATERIALIZE_CREATE_SQL: &str = "SELECT mz_is_superuser()::text AS super, has_system_privilege('CREATEDB')::text AS createdb,
       has_system_privilege('CREATEROLE')::text AS createrole, has_database_privilege({db}, 'CREATE')::text AS db_create";

/// `sql` (one of the `pg_roles` checks) also reading CREATE on the database.
fn with_db_create(sql: &str) -> String {
    sql.replacen(
        "\n  FROM pg_roles r",
        ",\n       has_database_privilege({db}, 'CREATE')::text AS db_create\n  FROM pg_roles r",
        1,
    )
}

/// Redshift and RisingWave.
const USER_SQL: &str = "SELECT usesuper::text AS super, usecreatedb::text AS createdb FROM pg_user WHERE usename = current_user";

const MATERIALIZE_SQL: &str = "SELECT mz_is_superuser()::text AS super, has_system_privilege('CREATEDB')::text AS createdb,
       has_system_privilege('CREATEROLE')::text AS createrole";

const CRATE_SQL: &str = "SELECT u.superuser::text AS super,
       EXISTS (SELECT 1 FROM sys.privileges p WHERE p.grantee = u.name AND p.class = 'CLUSTER'
                AND p.type = 'AL' AND p.state = 'GRANT')::text AS al
  FROM sys.users u WHERE u.name = CURRENT_USER";

/// H2 2.x, then 1.4 (the columns were renamed).
const H2_SQL: &str = "SELECT CAST(IS_ADMIN AS VARCHAR) AS super FROM INFORMATION_SCHEMA.USERS WHERE USER_NAME = CURRENT_USER";
const H2_OLD_SQL: &str = "SELECT CAST(ADMIN AS VARCHAR) AS super FROM INFORMATION_SCHEMA.USERS WHERE NAME = CURRENT_USER";

/// The answers of a check, by column name; a missing column or NULL is
/// `None` (the server couldn't tell).
#[derive(Debug, Default, Clone)]
pub(crate) struct Flags {
    pub super_: Option<bool>,
    pub createdb: Option<bool>,
    pub createrole: Option<bool>,
    pub sys_createdb: Option<bool>,
    pub sys_createrole: Option<bool>,
    /// May see other users' statements.
    pub stats: Option<bool>,
    /// May end other users' sessions.
    pub signal: Option<bool>,
    pub sys_backup: Option<bool>,
    pub sys_restore: Option<bool>,
    pub db_backup: Option<bool>,
    pub db_drop: Option<bool>,
    /// Owns the database (or is a member of its owner).
    pub owner: Option<bool>,
    /// CrateDB's AL on the cluster.
    pub al: Option<bool>,
    /// CREATE on the database (new schemas).
    pub db_create: Option<bool>,
}

fn boolean(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "1" => Some(true),
        "false" | "f" | "0" => Some(false),
        _ => None,
    }
}

impl Flags {
    fn from_row(r: &SimpleQueryRow) -> Self {
        let get = |name: &str| cell(r, name).as_deref().and_then(boolean);
        Flags {
            super_: get("super"),
            createdb: get("createdb"),
            createrole: get("createrole"),
            sys_createdb: get("sys_createdb"),
            sys_createrole: get("sys_createrole"),
            stats: get("stats"),
            signal: get("signal"),
            sys_backup: get("sys_backup"),
            sys_restore: get("sys_restore"),
            db_backup: get("db_backup"),
            db_drop: get("db_drop"),
            owner: get("owner"),
            al: get("al"),
            db_create: get("db_create"),
        }
    }
}

/// Allowed / denied, or unknown when the check gave nothing.
fn access(v: Option<bool>, missing: &str) -> Access {
    v.map_or(Access::Unknown, |ok| Access::check(ok, missing))
}

/// Allowed only when known true: a false can't tell (another way in exists
/// that can't be read).
fn only_allowed(v: Option<bool>) -> Access {
    if v == Some(true) {
        Access::Allowed
    } else {
        Access::Unknown
    }
}

fn any(v: &[Option<bool>]) -> Option<bool> {
    if v.contains(&Some(true)) {
        Some(true)
    } else if v.iter().all(Option::is_some) {
        Some(false)
    } else {
        None
    }
}

/// The actions the variant offers; `Unknown` for the others.
fn offered(v: Variant, p: Permissions) -> Permissions {
    let caps = crate::design::capabilities(v);
    let backup = crate::backup::spec(v);
    let keep = |on: bool, a: Access| if on { a } else { Access::Unknown };
    Permissions {
        backup: keep(backup.is_some(), p.backup),
        restore: keep(backup.is_some_and(|b| b.restore), p.restore),
        profiler: keep(crate::profiler::supported(v), p.profiler),
        kill_session: keep(caps.kill_session, p.kill_session),
        create_database: keep(caps.create_database, p.create_database),
        drop_database: keep(caps.drop_database, p.drop_database),
        manage_security: keep(crate::security::spec(v).is_some(), p.manage_security),
        create_schema: keep(crate::schemas::spec(v).is_some(), p.create_schema),
    }
}

/// The flags to what the UI allows. `drop_target`: the explorer named a
/// database (drop refers to it).
pub(crate) fn decide(v: Variant, f: &Flags, drop_target: bool) -> Permissions {
    let mut p = if f.super_ == Some(true) { Permissions::all() } else { rules(v, f) };
    if !drop_target {
        p.drop_database = Access::Unknown;
    }
    offered(v, p)
}

/// A login that isn't the server's administrator (or couldn't tell).
fn rules(v: Variant, f: &Flags) -> Permissions {
    // Denied only when the superuser check itself answered.
    let known = |a: Access| if f.super_.is_some() { a } else { Access::Unknown };
    match v {
        Variant::Cockroach => Permissions {
            backup: known(access(any(&[f.db_backup, f.sys_backup]), "privilegio BACKUP sobre la base (o rol admin)")),
            restore: known(access(
                any(&[f.sys_restore, f.createdb]),
                "privilegio de sistema RESTORE u opción CREATEDB (o rol admin)",
            )),
            profiler: only_allowed(f.stats),
            kill_session: only_allowed(f.signal),
            create_database: known(access(any(&[f.createdb, f.sys_createdb]), "opción CREATEDB (o rol admin)")),
            drop_database: known(access(any(&[f.owner, f.db_drop]), "ser el dueño de la base o tener DROP sobre ella (o rol admin)")),
            manage_security: known(access(any(&[f.createrole, f.sys_createrole]), "opción CREATEROLE (o rol admin)")),
            create_schema: known(access(f.db_create, "privilegio CREATE sobre la base (o rol admin)")),
        },
        Variant::Redshift | Variant::RisingWave => Permissions {
            create_database: known(access(f.createdb, "CREATEDB (o superusuario)")),
            create_schema: known(access(f.db_create, "privilegio CREATE sobre la base (o superusuario)")),
            ..Default::default()
        },
        Variant::Materialize => Permissions {
            create_database: known(access(f.createdb, "privilegio de sistema CREATEDB (o superusuario)")),
            manage_security: known(access(f.createrole, "privilegio de sistema CREATEROLE (o superusuario)")),
            create_schema: known(access(f.db_create, "privilegio CREATE sobre la base (o superusuario)")),
            ..Default::default()
        },
        Variant::CrateDb => {
            let al = only_allowed(f.al);
            Permissions { backup: al.clone(), restore: al.clone(), profiler: al.clone(), manage_security: al, ..Default::default() }
        }
        Variant::H2 => {
            let admin = || known(Access::check(false, "derechos de administrador (ADMIN)"));
            Permissions {
                backup: admin(),
                restore: admin(),
                profiler: admin(),
                kill_session: admin(),
                manage_security: admin(),
                ..Default::default()
            }
        }
        Variant::Denodo => Permissions::default(),
        _ => {
            let (stats, signal) = match v {
                Variant::OpenGauss => (
                    known(access(f.stats, "MONADMIN (o SYSADMIN)")),
                    known(Access::check(false, "SYSADMIN")),
                ),
                // Its own history and terminate function.
                Variant::Yellowbrick => (Access::Unknown, Access::Unknown),
                _ => (
                    known(access(f.stats, "rol pg_read_all_stats o pg_monitor (o superusuario)")),
                    known(access(f.signal, "rol pg_signal_backend (o superusuario)")),
                ),
            };
            let admin = if v == Variant::OpenGauss { "SYSADMIN" } else { "superusuario" };
            Permissions {
                profiler: stats,
                kill_session: signal,
                create_database: known(access(f.createdb, &format!("CREATEDB (o {admin})"))),
                drop_database: known(access(f.owner, &format!("ser el dueño de la base (o {admin})"))),
                manage_security: known(access(f.createrole, &format!("CREATEROLE (o {admin})"))),
                create_schema: known(access(f.db_create, &format!("privilegio CREATE sobre la base (o {admin})"))),
                ..Default::default()
            }
        }
    }
}

/// The first row of `sql`; `None` when the server refused it (or had no
/// row). Only a lost connection is an error.
async fn first_row(s: &PgSession, sql: &str) -> Result<Option<SimpleQueryRow>> {
    match s.text_within(sql, LIMIT).await {
        Ok(rows) => Ok(rows.into_iter().next()),
        Err(e @ Error::Connect(_)) => Err(e),
        Err(e) => {
            if s.client.is_closed() {
                return Err(Error::Connect("se cerró la conexión con el servidor".into()));
            }
            tracing::debug!("{:?}: permissions check refused: {e}", s.variant);
            Ok(None)
        }
    }
}

/// The first query that answers.
async fn flags(s: &PgSession, queries: &[String]) -> Result<Option<Flags>> {
    for sql in queries {
        if let Some(r) = first_row(s, sql).await? {
            return Ok(Some(Flags::from_row(&r)));
        }
    }
    Ok(None)
}

pub(crate) async fn check(s: &PgSession, database: Option<&str>) -> Result<Permissions> {
    let v = s.variant;
    let target = database.map(str::trim).filter(|d| !d.is_empty());
    let db = target.unwrap_or(&s.database);
    let fill = |sql: &str| sql.replace("{db}", &lit(v, db)).replace("{ident}", &quote_ident(Quote::Double, db));
    let queries: Vec<String> = match v {
        Variant::Denodo => return Ok(Permissions::default()),
        Variant::Cockroach => vec![
            fill(&with_db_create(COCKROACH_SQL)),
            fill(COCKROACH_SQL),
            fill(&with_db_create(COCKROACH_OLD_SQL)),
            fill(COCKROACH_OLD_SQL),
        ],
        Variant::OpenGauss => vec![fill(&with_db_create(OPENGAUSS_SQL)), fill(OPENGAUSS_SQL)],
        Variant::Redshift => vec![fill(REDSHIFT_SQL), USER_SQL.into()],
        Variant::RisingWave => vec![USER_SQL.into()],
        Variant::Materialize => vec![fill(MATERIALIZE_CREATE_SQL), MATERIALIZE_SQL.into()],
        Variant::CrateDb => vec![CRATE_SQL.into()],
        Variant::H2 => vec![H2_SQL.into(), H2_OLD_SQL.into()],
        _ => vec![fill(&with_db_create(PG_SQL)), fill(PG_SQL)],
    };
    Ok(flags(s, &queries).await?.map(|f| decide(v, &f, target.is_some())).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    fn pg(super_: bool, createdb: bool, createrole: bool, stats: bool, signal: bool, owner: bool) -> Flags {
        Flags {
            super_: Some(super_),
            createdb: Some(createdb),
            createrole: Some(createrole),
            stats: Some(stats),
            signal: Some(signal),
            owner: Some(owner),
            ..Default::default()
        }
    }

    #[test]
    fn booleans_in_every_spelling() {
        assert_eq!(boolean("true"), Some(true));
        assert_eq!(boolean("t"), Some(true));
        assert_eq!(boolean("TRUE"), Some(true));
        assert_eq!(boolean("false"), Some(false));
        assert_eq!(boolean("f"), Some(false));
        assert_eq!(boolean(""), None);
    }

    #[test]
    fn a_superuser_gets_what_the_variant_offers() {
        let f = pg(true, false, false, false, false, false);
        let p = decide(Variant::Postgres, &f, true);
        let expected = Permissions {
            profiler: Access::Allowed,
            kill_session: Access::Allowed,
            create_database: Access::Allowed,
            drop_database: Access::Allowed,
            manage_security: Access::Allowed,
            create_schema: Access::Allowed,
            ..Default::default()
        };
        assert_eq!(p, expected);
        assert_eq!(decide(Variant::Postgres, &f, false).drop_database, Access::Unknown);
        // No native backups on PostgreSQL itself; CockroachDB has them.
        assert_eq!(decide(Variant::Cockroach, &f, true), Permissions::all());
    }

    #[test]
    fn a_plain_postgres_role_is_denied_what_it_lacks() {
        let p = decide(Variant::Postgres, &pg(false, false, false, false, false, false), true);
        assert!(denied(&p.profiler, "pg_read_all_stats"));
        assert!(denied(&p.kill_session, "pg_signal_backend"));
        assert!(denied(&p.create_database, "CREATEDB"));
        assert!(denied(&p.drop_database, "dueño de la base"));
        assert!(denied(&p.manage_security, "CREATEROLE"));
        assert_eq!((p.backup, p.restore), (Access::Unknown, Access::Unknown));
        // CREATE on the database wasn't read (older query): unknown.
        assert_eq!(p.create_schema, Access::Unknown);
        let f = Flags { db_create: Some(false), ..pg(false, false, false, false, false, false) };
        assert!(denied(&decide(Variant::Postgres, &f, true).create_schema, "CREATE sobre la base"));
        let f = Flags { db_create: Some(true), ..f };
        assert_eq!(decide(Variant::Greenplum, &f, true).create_schema, Access::Allowed);
    }

    #[test]
    fn the_create_column_goes_into_every_pg_roles_check() {
        for sql in [PG_SQL, OPENGAUSS_SQL, COCKROACH_SQL, COCKROACH_OLD_SQL] {
            let with = with_db_create(sql);
            assert_ne!(with, sql);
            assert!(with.contains("AS db_create\n  FROM pg_roles r WHERE r.rolname = current_user"), "{with}");
        }
    }

    #[test]
    fn create_schema_only_where_schemas_are_created() {
        let f = Flags { super_: Some(true), ..Default::default() };
        for v in [Variant::CrateDb, Variant::Denodo] {
            assert_eq!(decide(v, &f, true).create_schema, Access::Unknown, "{v:?}");
        }
        let f = Flags { super_: Some(false), createdb: Some(false), db_create: Some(false), ..Default::default() };
        assert!(denied(&decide(Variant::Redshift, &f, true).create_schema, "CREATE"));
        assert!(denied(&decide(Variant::Materialize, &f, true).create_schema, "CREATE"));
        assert_eq!(decide(Variant::H2, &f, true).create_schema, Access::Unknown);
    }

    #[test]
    fn a_monitoring_owner_role_is_allowed() {
        let p = decide(Variant::Aurora, &pg(false, true, true, true, true, true), true);
        assert_eq!(p.profiler, Access::Allowed);
        assert_eq!(p.kill_session, Access::Allowed);
        assert_eq!(p.create_database, Access::Allowed);
        assert_eq!(p.drop_database, Access::Allowed);
        assert_eq!(p.manage_security, Access::Allowed);
    }

    #[test]
    fn nothing_known_leaves_everything_unknown() {
        for v in Variant::ALL {
            assert_eq!(decide(v, &Flags::default(), true), Permissions::default(), "{v:?}");
        }
    }

    #[test]
    fn opengauss_and_yellowbrick() {
        let p = decide(Variant::OpenGauss, &pg(false, false, false, true, false, true), true);
        assert_eq!(p.profiler, Access::Allowed);
        assert!(denied(&p.kill_session, "SYSADMIN"));
        assert_eq!(p.drop_database, Access::Allowed);
        let p = decide(Variant::Yellowbrick, &pg(false, false, false, false, false, false), true);
        assert_eq!((p.profiler, p.kill_session), (Access::Unknown, Access::Unknown));
        assert!(denied(&p.create_database, "CREATEDB"));
    }

    #[test]
    fn cockroach_grants_and_options() {
        let limited = Flags {
            super_: Some(false),
            createdb: Some(false),
            createrole: Some(false),
            sys_createdb: Some(false),
            sys_createrole: Some(false),
            stats: Some(false),
            signal: Some(false),
            sys_backup: Some(false),
            sys_restore: Some(false),
            db_backup: Some(false),
            db_drop: Some(false),
            owner: Some(false),
            al: None,
            db_create: Some(false),
        };
        let p = decide(Variant::Cockroach, &limited, true);
        assert!(denied(&p.backup, "BACKUP"));
        assert!(denied(&p.restore, "RESTORE"));
        assert!(denied(&p.create_database, "CREATEDB"));
        assert!(denied(&p.drop_database, "dueño"));
        assert!(denied(&p.manage_security, "CREATEROLE"));
        // VIEWACTIVITY / CANCELQUERY as role options can't be read.
        assert_eq!((p.profiler.clone(), p.kill_session.clone()), (Access::Unknown, Access::Unknown));

        let owner = Flags { createdb: Some(true), db_backup: Some(true), owner: Some(true), stats: Some(true), ..limited.clone() };
        let p = decide(Variant::Cockroach, &owner, true);
        assert_eq!((p.backup, p.restore, p.drop_database, p.profiler), (Access::Allowed, Access::Allowed, Access::Allowed, Access::Allowed));

        // Before system privileges: only what the old query read.
        let old = Flags { super_: Some(false), createdb: Some(false), createrole: Some(true), owner: Some(false), ..Default::default() };
        let p = decide(Variant::Cockroach, &old, true);
        assert_eq!((p.backup, p.restore), (Access::Unknown, Access::Unknown));
        assert_eq!(p.manage_security, Access::Allowed);
        // Not the owner, but the DROP grant wasn't read.
        assert_eq!(p.drop_database, Access::Unknown);
    }

    #[test]
    fn user_catalog_engines() {
        let f = Flags { super_: Some(false), createdb: Some(false), ..Default::default() };
        let p = decide(Variant::Redshift, &f, true);
        assert!(denied(&p.create_database, "CREATEDB"));
        assert_eq!((p.profiler, p.kill_session, p.drop_database, p.manage_security), Default::default());
        let p = decide(Variant::RisingWave, &Flags { super_: Some(true), ..f }, true);
        // Neither profiler nor kill on RisingWave.
        assert_eq!((p.profiler, p.kill_session), (Access::Unknown, Access::Unknown));
        assert_eq!((p.create_database, p.drop_database, p.manage_security), (Access::Allowed, Access::Allowed, Access::Allowed));
    }

    #[test]
    fn crate_and_h2() {
        let p = decide(Variant::CrateDb, &Flags { super_: Some(false), al: Some(true), ..Default::default() }, true);
        assert_eq!((p.backup, p.restore, p.profiler, p.manage_security), (Access::Allowed, Access::Allowed, Access::Allowed, Access::Allowed));
        assert_eq!((p.create_database, p.kill_session), (Access::Unknown, Access::Unknown));
        let p = decide(Variant::CrateDb, &Flags { super_: Some(false), al: Some(false), ..Default::default() }, true);
        assert_eq!(p, Permissions::default());

        let p = decide(Variant::H2, &Flags { super_: Some(false), ..Default::default() }, true);
        assert!(denied(&p.backup, "ADMIN"));
        assert!(denied(&p.kill_session, "ADMIN"));
        assert_eq!((p.create_database, p.drop_database), (Access::Unknown, Access::Unknown));
        let p = decide(Variant::H2, &Flags { super_: Some(true), ..Default::default() }, true);
        assert_eq!((p.backup, p.restore, p.profiler, p.kill_session, p.manage_security), (Access::Allowed, Access::Allowed, Access::Allowed, Access::Allowed, Access::Allowed));
    }
}

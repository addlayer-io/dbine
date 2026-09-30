//! `Session::permissions` against real servers, as the administrator and
//! as limited logins the test creates (and drops). Each test reads
//! `DBINE_TEST_<ENGINE>_URL` (`postgres://user:pass@host:port/db`) and is
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_H2_URL=postgres://sa:sa@localhost:25025/test \
//! DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres' \
//! DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
//!   cargo test -p dbine-driver-postgres --test permissions -- --ignored
//! ```

use dbine_driver::{Access, ConnectionConfig, Driver, Permissions, QueryOutcome, Session};
use std::sync::Arc;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str, check: bool) {
    let mut out = QueryOutcome::default();
    let r = s.execute(sql, 100, &mut out).await;
    if check {
        r.unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    }
}

async fn as_user(d: &Arc<dyn Driver>, admin: &ConnectionConfig, user: &str, pass: Option<&str>) -> Box<dyn Session> {
    let cfg = ConnectionConfig { username: Some(user.into()), password: pass.map(Into::into), ..admin.clone() };
    d.connect(&cfg, None).await.unwrap_or_else(|e| panic!("connect as {user}: {e}"))
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

const ADMIN_PG: Permissions = Permissions {
    backup: Access::Unknown,
    restore: Access::Unknown,
    profiler: Access::Allowed,
    kill_session: Access::Allowed,
    create_database: Access::Allowed,
    drop_database: Access::Allowed,
    manage_security: Access::Allowed,
};

#[tokio::test]
#[ignore]
async fn postgres() {
    let env = "DBINE_TEST_POSTGRES_URL";
    let Some(admin) = cfg("postgres", env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver("postgres");
    let mut s = d.connect(&admin, None).await.unwrap();
    assert_eq!(s.permissions(Some("postgres")).await.unwrap(), ADMIN_PG);
    assert_eq!(s.permissions(None).await.unwrap().drop_database, Access::Unknown);

    let cleanup = ["DROP DATABASE IF EXISTS dbine_perm_db", "DROP ROLE IF EXISTS dbine_perm_lim", "DROP ROLE IF EXISTS dbine_perm_opt"];
    for sql in cleanup {
        run(&mut s, sql, true).await;
    }
    run(&mut s, "CREATE ROLE dbine_perm_lim LOGIN PASSWORD 'pw'", true).await;
    run(&mut s, "CREATE ROLE dbine_perm_opt LOGIN PASSWORD 'pw' CREATEDB CREATEROLE IN ROLE pg_monitor, pg_signal_backend", true).await;
    run(&mut s, "CREATE DATABASE dbine_perm_db OWNER dbine_perm_opt", true).await;

    let mut lim = as_user(&d, &admin, "dbine_perm_lim", Some("pw")).await;
    let p = lim.permissions(Some("dbine_perm_db")).await.unwrap();
    eprintln!("postgres limited: {p:?}");
    assert!(denied(&p.profiler, "pg_read_all_stats"));
    assert!(denied(&p.kill_session, "pg_signal_backend"));
    assert!(denied(&p.create_database, "CREATEDB"));
    assert!(denied(&p.drop_database, "dueño"));
    assert!(denied(&p.manage_security, "CREATEROLE"));
    drop(lim);

    let mut opt = as_user(&d, &admin, "dbine_perm_opt", Some("pw")).await;
    assert_eq!(opt.permissions(Some("dbine_perm_db")).await.unwrap(), ADMIN_PG);
    assert!(denied(&opt.permissions(Some("postgres")).await.unwrap().drop_database, "dueño"));
    drop(opt);

    for sql in cleanup {
        run(&mut s, sql, true).await;
    }
}

#[tokio::test]
#[ignore]
async fn cockroach() {
    let env = "DBINE_TEST_COCKROACH_URL";
    let Some(admin) = cfg("cockroachdb", env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver("cockroachdb");
    let mut s = d.connect(&admin, None).await.unwrap();
    assert_eq!(s.permissions(Some("defaultdb")).await.unwrap(), Permissions::all());

    let cleanup = ["DROP DATABASE IF EXISTS dbine_perm_db CASCADE", "DROP USER IF EXISTS dbine_perm_lim", "DROP USER IF EXISTS dbine_perm_opt"];
    // System privileges keep a user from being dropped (the users may not exist).
    for u in ["dbine_perm_lim", "dbine_perm_opt"] {
        run(&mut s, &format!("REVOKE SYSTEM ALL FROM {u}"), false).await;
    }
    for sql in cleanup {
        run(&mut s, sql, true).await;
    }
    run(&mut s, "CREATE USER dbine_perm_lim", true).await;
    run(&mut s, "CREATE USER dbine_perm_opt WITH CREATEDB CREATEROLE", true).await;
    run(&mut s, "GRANT SYSTEM VIEWACTIVITY, CANCELQUERY TO dbine_perm_opt", true).await;
    run(&mut s, "CREATE DATABASE dbine_perm_db", true).await;
    run(&mut s, "ALTER DATABASE dbine_perm_db OWNER TO dbine_perm_opt", true).await;

    let mut lim = as_user(&d, &admin, "dbine_perm_lim", None).await;
    let p = lim.permissions(Some("dbine_perm_db")).await.unwrap();
    eprintln!("cockroach limited: {p:?}");
    assert!(denied(&p.backup, "BACKUP"));
    assert!(denied(&p.restore, "RESTORE"));
    assert!(denied(&p.create_database, "CREATEDB"));
    assert!(denied(&p.drop_database, "dueño"));
    assert!(denied(&p.manage_security, "CREATEROLE"));
    assert_eq!((p.profiler, p.kill_session), (Access::Unknown, Access::Unknown));
    drop(lim);

    let mut opt = as_user(&d, &admin, "dbine_perm_opt", None).await;
    assert_eq!(opt.permissions(Some("dbine_perm_db")).await.unwrap(), Permissions::all());
    let other = opt.permissions(Some("defaultdb")).await.unwrap();
    eprintln!("cockroach owner on defaultdb: {other:?}");
    assert!(denied(&other.backup, "BACKUP"));
    assert!(denied(&other.drop_database, "dueño"));
    drop(opt);

    run(&mut s, "REVOKE SYSTEM ALL FROM dbine_perm_opt", true).await;
    for sql in cleanup {
        run(&mut s, sql, true).await;
    }
}

#[tokio::test]
#[ignore]
async fn h2() {
    let env = "DBINE_TEST_H2_URL";
    let Some(admin) = cfg("h2", env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver("h2");
    let mut s = d.connect(&admin, None).await.unwrap();
    let all = Permissions { create_database: Access::Unknown, drop_database: Access::Unknown, ..Permissions::all() };
    assert_eq!(s.permissions(None).await.unwrap(), all);

    run(&mut s, "DROP USER IF EXISTS DBINE_PERM_LIM", true).await;
    run(&mut s, "CREATE USER DBINE_PERM_LIM PASSWORD 'pw'", true).await;
    let mut lim = as_user(&d, &admin, "DBINE_PERM_LIM", Some("pw")).await;
    let p = lim.permissions(None).await.unwrap();
    eprintln!("h2 limited: {p:?}");
    for a in [&p.backup, &p.restore, &p.profiler, &p.kill_session, &p.manage_security] {
        assert!(denied(a, "ADMIN"), "{p:?}");
    }
    drop(lim);
    run(&mut s, "DROP USER IF EXISTS DBINE_PERM_LIM", false).await;
}

/// `DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres'`
/// (the image's user is SYSADMIN, not superuser).
#[tokio::test]
#[ignore]
async fn opengauss() {
    let env = "DBINE_TEST_OPENGAUSS_URL";
    let Some(admin) = cfg("opengauss", env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver("opengauss");
    let mut s = d.connect(&admin, None).await.unwrap();
    assert_eq!(s.permissions(Some("postgres")).await.unwrap(), ADMIN_PG);

    run(&mut s, "DROP USER IF EXISTS dbine_perm_lim", true).await;
    run(&mut s, "CREATE USER dbine_perm_lim PASSWORD 'Dbine@1234'", true).await;
    // The new user's password only has openGauss's sha256 hash, which the
    // client can't log in with: become it in this session instead.
    run(&mut s, "SET ROLE dbine_perm_lim PASSWORD 'Dbine@1234'", true).await;
    let p = s.permissions(Some("postgres")).await.unwrap();
    eprintln!("opengauss limited: {p:?}");
    assert!(denied(&p.profiler, "MONADMIN"));
    assert!(denied(&p.kill_session, "SYSADMIN"));
    assert!(denied(&p.create_database, "CREATEDB"));
    assert!(denied(&p.drop_database, "dueño"));
    assert!(denied(&p.manage_security, "CREATEROLE"));
    run(&mut s, "RESET ROLE", true).await;
    run(&mut s, "DROP USER IF EXISTS dbine_perm_lim", true).await;
}

/// `DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize`
/// (the image's user has CREATEDB and CREATEROLE but isn't superuser).
#[tokio::test]
#[ignore]
async fn materialize() {
    let env = "DBINE_TEST_MATERIALIZE_URL";
    let Some(admin) = cfg("materialize", env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver("materialize");
    let mut s = d.connect(&admin, None).await.unwrap();
    let p = s.permissions(Some("materialize")).await.unwrap();
    eprintln!("materialize: {p:?}");
    assert_eq!((p.create_database, p.manage_security), (Access::Allowed, Access::Allowed));
    assert_eq!(p.kill_session, Access::Unknown);

    run(&mut s, "DROP ROLE IF EXISTS dbine_perm_lim", true).await;
    run(&mut s, "CREATE ROLE dbine_perm_lim", true).await;
    let mut lim = as_user(&d, &admin, "dbine_perm_lim", None).await;
    let p = lim.permissions(Some("materialize")).await.unwrap();
    eprintln!("materialize limited: {p:?}");
    assert!(denied(&p.create_database, "CREATEDB"));
    assert!(denied(&p.manage_security, "CREATEROLE"));
    drop(lim);
    run(&mut s, "DROP ROLE IF EXISTS dbine_perm_lim", true).await;
}

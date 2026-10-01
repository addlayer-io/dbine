//! What the login may do (`Session::permissions`) against real servers,
//! as root and as an account with a few grants through a role
//! (`DBINE_TEST_MYSQL_URL`, `DBINE_TEST_MARIADB_URL`, `DBINE_TEST_TIDB_URL`,
//! `DBINE_TEST_STARROCKS_URL`, see tests/integration.rs):
//! `cargo test -p dbine-driver-mysql --test permissions -- --ignored`

use dbine_driver::{Access, ConnectionConfig, Driver, Permissions, QueryOutcome, Session};
use std::sync::Arc;

const PASSWORD: &str = "Pw_12345!x";

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

/// The driver, root's config and the limited account's config.
fn setup(id: &str, env: &str) -> Option<(Arc<dyn Driver>, ConnectionConfig, ConnectionConfig)> {
    let Some(root) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let d = dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let limited = ConnectionConfig { username: Some("dbine_perm".into()), password: Some(PASSWORD.into()), ..root.clone() };
    Some((d, root, limited))
}

async fn check(d: &Arc<dyn Driver>, cfg: &ConnectionConfig, db: Option<&str>) -> Permissions {
    let mut s = d.connect(cfg, None).await.unwrap();
    s.permissions(db).await.unwrap()
}

#[tokio::test]
#[ignore]
async fn mysql() {
    let Some((d, root, limited)) = setup("mysql", "DBINE_TEST_MYSQL_URL") else { return };
    let p = check(&d, &root, Some("dbine_perm")).await;
    assert_eq!(p, Permissions { restore: Access::Unknown, create_schema: Access::Unknown, ..Permissions::all() }, "root");
    assert_eq!(check(&d, &root, None).await.drop_database, Access::Unknown);

    let mut admin = d.connect(&root, None).await.unwrap();
    let cleanup = "DROP USER IF EXISTS 'dbine_perm'@'%'; DROP ROLE IF EXISTS 'dbine_perm_r'";
    run(&mut admin, cleanup).await;
    run(
        &mut admin,
        &format!(
            "CREATE USER 'dbine_perm'@'%' IDENTIFIED BY '{PASSWORD}'; CREATE ROLE 'dbine_perm_r'; \
             GRANT PROCESS, BACKUP_ADMIN ON *.* TO 'dbine_perm_r'; \
             GRANT CREATE, DROP ON `dbine\\_perm`.* TO 'dbine_perm_r'; \
             GRANT 'dbine_perm_r' TO 'dbine_perm'@'%'; SET DEFAULT ROLE ALL TO 'dbine_perm'@'%'"
        ),
    )
    .await;
    let p = check(&d, &limited, Some("dbine_perm")).await;
    run(&mut admin, cleanup).await;
    // Through the role (SHOW GRANTS expands active roles).
    assert_eq!((&p.backup, &p.profiler, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed), "{p:?}");
    assert_eq!(p.create_database, Access::Unknown, "CREATE only on a database pattern");
    assert!(denied(&p.kill_session, "CONNECTION_ADMIN"), "{p:?}");
    assert!(denied(&p.manage_security, "CREATE USER"), "{p:?}");
    assert_eq!(p.restore, Access::Unknown);
}

#[tokio::test]
#[ignore]
async fn mariadb() {
    let Some((d, root, limited)) = setup("mariadb", "DBINE_TEST_MARIADB_URL") else { return };
    let p = check(&d, &root, Some("dbine_perm")).await;
    assert_eq!(p, Permissions { backup: Access::Unknown, restore: Access::Unknown, create_schema: Access::Unknown, ..Permissions::all() }, "root");

    let mut admin = d.connect(&root, None).await.unwrap();
    let cleanup = "DROP USER IF EXISTS 'dbine_perm'@'%'; DROP ROLE IF EXISTS dbine_perm_r";
    run(&mut admin, cleanup).await;
    run(
        &mut admin,
        &format!(
            "CREATE USER 'dbine_perm'@'%' IDENTIFIED BY '{PASSWORD}'; CREATE ROLE dbine_perm_r; \
             GRANT PROCESS, CONNECTION ADMIN ON *.* TO dbine_perm_r; \
             GRANT DROP ON `dbine\\_perm`.* TO dbine_perm_r; \
             GRANT dbine_perm_r TO 'dbine_perm'@'%'; SET DEFAULT ROLE dbine_perm_r FOR 'dbine_perm'@'%'"
        ),
    )
    .await;
    let p = check(&d, &limited, Some("dbine_perm")).await;
    let other = check(&d, &limited, Some("otra")).await;
    run(&mut admin, cleanup).await;
    assert_eq!((&p.kill_session, &p.profiler, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed), "{p:?}");
    assert!(denied(&other.drop_database, "DROP"), "{other:?}");
    assert!(denied(&p.create_database, "CREATE"), "{p:?}");
    assert!(denied(&p.manage_security, "CREATE USER"), "{p:?}");
    assert_eq!((&p.backup, &p.restore), (&Access::Unknown, &Access::Unknown));
}

#[tokio::test]
#[ignore]
async fn tidb() {
    let Some((d, root, limited)) = setup("tidb", "DBINE_TEST_TIDB_URL") else { return };
    let p = check(&d, &root, Some("dbine_perm")).await;
    assert_eq!(p, Permissions { create_schema: Access::Unknown, ..Permissions::all() }, "root");

    let mut admin = d.connect(&root, None).await.unwrap();
    let cleanup = "DROP USER IF EXISTS 'dbine_perm'@'%'; DROP ROLE IF EXISTS 'dbine_perm_r'";
    run(&mut admin, cleanup).await;
    run(
        &mut admin,
        &format!(
            "CREATE USER 'dbine_perm'@'%' IDENTIFIED BY '{PASSWORD}'; CREATE ROLE 'dbine_perm_r'; \
             GRANT PROCESS, BACKUP_ADMIN ON *.* TO 'dbine_perm_r'; \
             GRANT CREATE, DROP ON `dbine_perm`.* TO 'dbine_perm_r'; \
             GRANT 'dbine_perm_r' TO 'dbine_perm'@'%'; SET DEFAULT ROLE ALL TO 'dbine_perm'@'%'"
        ),
    )
    .await;
    let p = check(&d, &limited, Some("dbine_perm")).await;
    run(&mut admin, cleanup).await;
    assert_eq!((&p.backup, &p.profiler, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed), "{p:?}");
    assert!(denied(&p.restore, "RESTORE_ADMIN"), "{p:?}");
    assert!(denied(&p.kill_session, "CONNECTION_ADMIN"), "{p:?}");
    assert!(denied(&p.manage_security, "CREATE USER"), "{p:?}");
    assert_eq!(p.create_database, Access::Unknown);
}

#[tokio::test]
#[ignore]
async fn starrocks() {
    let Some((d, root, limited)) = setup("starrocks", "DBINE_TEST_STARROCKS_URL") else { return };
    let p = check(&d, &root, Some("dbine_perm")).await;
    assert_eq!(p, Permissions { kill_session: Access::Unknown, ..Permissions::all() }, "root");

    let mut admin = d.connect(&root, None).await.unwrap();
    let cleanup = "DROP USER IF EXISTS 'dbine_perm'@'%'";
    run(&mut admin, cleanup).await;
    run(&mut admin, &format!("CREATE USER 'dbine_perm'@'%' IDENTIFIED BY '{PASSWORD}' DEFAULT ROLE 'db_admin'")).await;
    let p = check(&d, &limited, Some("dbine_perm")).await;
    run(&mut admin, cleanup).await;
    assert_eq!((&p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed), "{p:?}");
    // Only the built-in roles are read, and only to allow.
    assert_eq!((&p.backup, &p.manage_security), (&Access::Unknown, &Access::Unknown), "{p:?}");
}

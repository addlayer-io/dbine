//! What a login may do (`Session::permissions`) against a real server, as a
//! DBA (`DBINE_TEST_ORACLE_ADMIN_URL`) and as a limited user created here:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test permissions -- --ignored
//! ```

use dbine_driver::{Access, ConnectionConfig, Permissions, QueryOutcome, Session};

fn config(env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    Some(cfg)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(sql, 100, &mut out).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn dba_and_limited_user() {
    let Some(cfg) = config("DBINE_TEST_ORACLE_ADMIN_URL") else { return };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let p = admin.permissions(Some("HR")).await.unwrap();
    println!("system: {p:?}");
    assert_eq!(p, Permissions::all());

    try_run(&mut admin, "DROP USER DBINE_PERM CASCADE").await;
    run(&mut admin, "CREATE USER DBINE_PERM IDENTIFIED BY \"Perm_123\" QUOTA 10M ON USERS").await;
    run(&mut admin, "GRANT CREATE SESSION, CREATE TABLE TO DBINE_PERM").await;

    let mut user_cfg = cfg.clone();
    user_cfg.username = Some("DBINE_PERM".into());
    user_cfg.password = Some("Perm_123".into());
    let mut u = d.connect(&user_cfg, None).await.unwrap();
    let own = u.permissions(Some("DBINE_PERM")).await.unwrap();
    println!("own schema: {own:?}");
    assert_eq!((own.backup, own.restore), (Access::Allowed, Access::Allowed));
    let other = u.permissions(Some("SYSTEM")).await.unwrap();
    println!("another schema: {other:?}");
    assert_eq!(other.backup, Access::Denied { missing: "DATAPUMP_EXP_FULL_DATABASE".into() });
    assert_eq!(other.restore, Access::Denied { missing: "DATAPUMP_IMP_FULL_DATABASE".into() });
    assert!(other.profiler.is_denied());
    assert_eq!(other.kill_session, Access::Denied { missing: "ALTER SYSTEM".into() });
    assert_eq!(other.create_database, Access::Denied { missing: "CREATE USER".into() });
    assert_eq!(other.drop_database, Access::Denied { missing: "DROP USER".into() });
    assert!(other.manage_security.is_denied());
    drop(u);

    // Roles are enabled at login: a new session sees them.
    run(&mut admin, "GRANT SELECT_CATALOG_ROLE, DATAPUMP_EXP_FULL_DATABASE TO DBINE_PERM").await;
    let mut u = d.connect(&user_cfg, None).await.unwrap();
    let p = u.permissions(Some("SYSTEM")).await.unwrap();
    println!("with roles: {p:?}");
    assert_eq!((p.profiler, p.backup), (Access::Allowed, Access::Allowed));
    assert!(p.restore.is_denied());
    drop(u);

    run(&mut admin, "DROP USER DBINE_PERM CASCADE").await;
}

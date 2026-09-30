//! What a login may do (`Session::permissions`) against a real server, as
//! `sa` and as a limited login created here (`DBINE_TEST_SQLSERVER_URL`,
//! see tests/integration.rs):
//! `cargo test -p dbine-driver-sqlserver --test permissions -- --ignored`

use dbine_driver::{Access, ConnectionConfig, Permissions, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn admin_and_limited_login() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    assert_eq!(admin.permissions(Some("master")).await.unwrap(), Permissions::all());

    run(
        &mut admin,
        "IF DB_ID('dbine_perm') IS NOT NULL BEGIN ALTER DATABASE dbine_perm SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_perm; END; \
         IF SUSER_ID('dbine_perm_ana') IS NOT NULL DROP LOGIN dbine_perm_ana; \
         IF SUSER_ID('dbine_perm_bea') IS NOT NULL DROP LOGIN dbine_perm_bea; \
         CREATE DATABASE dbine_perm; \
         CREATE LOGIN dbine_perm_ana WITH PASSWORD = 'Pw_perm_123!', CHECK_POLICY = OFF; \
         CREATE LOGIN dbine_perm_bea WITH PASSWORD = 'Pw_perm_123!', CHECK_POLICY = OFF;",
    )
    .await;
    run(
        &mut admin,
        "USE dbine_perm; CREATE USER dbine_perm_ana FOR LOGIN dbine_perm_ana; ALTER ROLE db_backupoperator ADD MEMBER dbine_perm_ana; \
         USE master; ALTER AUTHORIZATION ON DATABASE::dbine_perm TO dbine_perm_bea; \
         GRANT VIEW SERVER STATE TO dbine_perm_bea;",
    )
    .await;

    // A backup operator: backups only.
    let mut ana_cfg = cfg.clone();
    ana_cfg.username = Some("dbine_perm_ana".into());
    ana_cfg.password = Some("Pw_perm_123!".into());
    let mut ana = d.connect(&ana_cfg, Some("dbine_perm")).await.unwrap();
    let p = ana.permissions(Some("dbine_perm")).await.unwrap();
    println!("ana: {p:?}");
    assert_eq!(p.backup, Access::Allowed);
    assert!(p.restore.is_denied());
    assert_eq!(p.profiler, Access::Denied { missing: "VIEW SERVER STATE".into() });
    assert_eq!(p.kill_session, Access::Denied { missing: "ALTER ANY CONNECTION".into() });
    assert_eq!(p.create_database, Access::Denied { missing: "CREATE ANY DATABASE".into() });
    assert!(p.drop_database.is_denied());
    assert!(p.manage_security.is_denied());
    drop(ana);

    // The database's owner: backs it up, restores and drops it, manages its
    // users; VIEW SERVER STATE for the profiler.
    let mut bea_cfg = ana_cfg.clone();
    bea_cfg.username = Some("dbine_perm_bea".into());
    let mut bea = d.connect(&bea_cfg, Some("master")).await.unwrap();
    let p = bea.permissions(Some("dbine_perm")).await.unwrap();
    println!("bea: {p:?}");
    assert_eq!((p.backup, p.restore, p.drop_database, p.manage_security), (Access::Allowed, Access::Allowed, Access::Allowed, Access::Allowed));
    assert_eq!(p.profiler, Access::Allowed);
    assert!(p.kill_session.is_denied());
    assert!(p.create_database.is_denied());
    // Server level: nothing to drop.
    assert_eq!(bea.permissions(None).await.unwrap().drop_database, Access::Unknown);
    drop(bea);

    run(
        &mut admin,
        "ALTER DATABASE dbine_perm SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_perm; \
         DROP LOGIN dbine_perm_ana; DROP LOGIN dbine_perm_bea;",
    )
    .await;
}

//! Users, roles and permissions against a real server
//! (`DBINE_TEST_SQLSERVER_URL`, see tests/integration.rs):
//! `cargo test -p dbine-driver-sqlserver --test security -- --ignored`

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg() -> Option<ConnectionConfig> {
    cfg_from(&std::env::var("DBINE_TEST_SQLSERVER_URL").ok()?)
}

fn cfg_from(url: &str) -> Option<ConnectionConfig> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
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
async fn users_roles_and_grants() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    run(&mut admin, "IF DB_ID('dbine_sec') IS NOT NULL BEGIN ALTER DATABASE dbine_sec SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_sec; END; IF SUSER_ID('dbine_ana') IS NOT NULL DROP LOGIN dbine_ana; CREATE DATABASE dbine_sec;").await;
    let mut s = d.connect(&cfg, Some("dbine_sec")).await.unwrap();
    run(&mut s, "CREATE TABLE dbo.facturas (id int PRIMARY KEY, total money);").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "lectores".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "lectores".into(), member: "dbine_ana".into() })).await;
    let table = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "facturas".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: Some(table.clone()), to: "lectores".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["UPDATE".into()], object: Some(table.clone()), to: "dbine_ana".into(), grantable: true })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["lectores".to_string()]);
    assert!(!ana.system);
    assert!(all.iter().any(|p| p.name == "lectores" && p.kind == PrincipalKind::Role));
    assert!(all.iter().any(|p| p.name == "dbo" && p.system));

    let g = s.grants("dbine_ana").await.unwrap();
    let upd = g.iter().find(|x| x.privilege == "UPDATE").expect("direct UPDATE");
    assert_eq!((upd.object.as_deref(), upd.object_kind.as_deref(), upd.grantable, upd.via.as_deref()), (Some("dbo.facturas"), Some("table"), true, None));
    let sel = g.iter().find(|x| x.privilege == "SELECT").expect("SELECT through the role");
    assert_eq!(sel.via.as_deref(), Some("lectores"));

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["UPDATE".into()], object: Some(table), from: "dbine_ana".into() })).await;
    assert!(s.grants("dbine_ana").await.unwrap().iter().all(|x| x.privilege != "UPDATE"));
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987!".into() })).await;
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(true));
    run(&mut s, &script(SecurityAction::RemoveMember { role: "lectores".into(), member: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "lectores".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana" && p.name != "lectores"));
    drop(s);
    run(&mut admin, "ALTER DATABASE dbine_sec SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_sec; DROP LOGIN dbine_ana;").await;
}

/// Babelfish through its TDS port (`DBINE_TEST_BABELFISH_URL`, see
/// tests/integration.rs): its GRANTs land in PostgreSQL's ACLs.
/// `cargo test -p dbine-driver-sqlserver --test security babelfish -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn babelfish_users_roles_and_grants() {
    let Some(mut cfg) = std::env::var("DBINE_TEST_BABELFISH_URL").ok().and_then(|u| cfg_from(&u)) else {
        return;
    };
    cfg.driver = "babelfish".into();
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "babelfish").unwrap();
    let spec = d.security().expect("Babelfish manages users");
    assert!(!spec.object_kinds.contains(&""));
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    run(&mut admin, "IF DB_ID('dbine_sec') IS NOT NULL DROP DATABASE dbine_sec;").await;
    run(&mut admin, "IF SUSER_ID('dbine_ana') IS NOT NULL DROP LOGIN dbine_ana;").await;
    run(&mut admin, "CREATE DATABASE dbine_sec;").await;
    let mut s = d.connect(&cfg, Some("dbine_sec")).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE dbo.facturas (id int PRIMARY KEY, total money);
         GO
         CREATE PROCEDURE dbo.cerrar AS SELECT 1
         GO
         CREATE SCHEMA ventas
         GO
         CREATE TABLE ventas.pedidos (id int)",
    )
    .await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "lectores".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "lectores".into(), member: "dbine_ana".into() })).await;
    let table = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "facturas".into() };
    let proc = ObjectRef { kind: "procedure".into(), schema: Some("dbo".into()), name: "cerrar".into() };
    let ventas = ObjectRef { kind: "schema".into(), schema: None, name: "ventas".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: Some(table.clone()), to: "lectores".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["EXECUTE".into()], object: Some(proc), to: "lectores".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["UPDATE".into()], object: Some(table.clone()), to: "dbine_ana".into(), grantable: true })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["INSERT".into()], object: Some(ventas.clone()), to: "dbine_ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!((ana.kind, ana.disabled, ana.system), (PrincipalKind::User, Some(false), false));
    assert_eq!(ana.member_of, vec!["lectores".to_string()]);
    assert!(ana.details.iter().any(|(k, v)| k == "Login" && v == "dbine_ana"));
    assert!(all.iter().any(|p| p.name == "lectores" && p.kind == PrincipalKind::Role && !p.system));
    assert!(all.iter().any(|p| p.name == "dbo" && p.superuser == Some(true)));
    assert!(all.iter().any(|p| p.name == "db_datareader" && p.system));

    let g = s.grants("dbine_ana").await.unwrap();
    let find = |p: &str, o: &str| g.iter().find(|x| x.privilege == p && x.object.as_deref() == Some(o)).unwrap_or_else(|| panic!("{p} on {o} in {g:?}"));
    let upd = find("UPDATE", "dbo.facturas");
    assert_eq!((upd.object_kind.as_deref(), upd.grantable, upd.via.as_deref()), (Some("table"), true, None));
    assert_eq!(find("SELECT", "dbo.facturas").via.as_deref(), Some("lectores"));
    let exec = find("EXECUTE", "dbo.cerrar");
    assert_eq!((exec.object_kind.as_deref(), exec.via.as_deref()), (Some("procedure"), Some("lectores")));
    // The schema's GRANT reaches its tables.
    assert_eq!(find("INSERT", "ventas.pedidos").via, None);

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["UPDATE".into()], object: Some(table), from: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["INSERT".into()], object: Some(ventas), from: "dbine_ana".into() })).await;
    let g = s.grants("dbine_ana").await.unwrap();
    assert!(g.iter().all(|x| x.via.is_some()), "{g:?}");
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987!".into() })).await;
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(true));
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: true })).await;
    run(&mut s, &script(SecurityAction::RemoveMember { role: "lectores".into(), member: "dbine_ana".into() })).await;
    assert!(s.grants("dbine_ana").await.unwrap().is_empty());
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "lectores".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana" && p.name != "lectores"));
    drop(s);
    run(&mut admin, "DROP DATABASE dbine_sec;").await;
    run(&mut admin, "DROP LOGIN dbine_ana;").await;
}

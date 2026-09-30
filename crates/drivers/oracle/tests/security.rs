//! Users, roles and permissions against a real server, as a DBA
//! (`DBINE_TEST_ORACLE_ADMIN_URL`) and, for the fallback without the DBA_
//! views, as a plain user (`DBINE_TEST_ORACLE_URL`, see tests/integration.rs):
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//! DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test security -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

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
async fn users_roles_and_grants() {
    let Some(cfg) = config("DBINE_TEST_ORACLE_ADMIN_URL") else { return };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    try_run(&mut s, "DROP USER DBINE_ANA CASCADE").await;
    try_run(&mut s, "DROP ROLE DBINE_LECT").await;
    try_run(&mut s, "DROP USER DBINE_SEC CASCADE").await;
    run(&mut s, "CREATE USER DBINE_SEC NO AUTHENTICATION QUOTA UNLIMITED ON USERS").await;
    run(&mut s, "CREATE TABLE DBINE_SEC.FACTURAS (ID NUMBER PRIMARY KEY, TOTAL NUMBER(10,2))").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "DBINE_LECT".into(), member: "DBINE_ANA".into() })).await;
    let table = ObjectRef { kind: "table".into(), schema: Some("DBINE_SEC".into()), name: "FACTURAS".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: Some(table.clone()), to: "DBINE_LECT".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["UPDATE".into()], object: Some(table.clone()), to: "DBINE_ANA".into(), grantable: true })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["CREATE VIEW".into()], object: None, to: "DBINE_ANA".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "DBINE_ANA").expect("the user");
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["DBINE_LECT".to_string()]);
    assert_eq!((ana.disabled, ana.can_login, ana.system), (Some(false), Some(true), false));
    assert!(all.iter().any(|p| p.name == "DBINE_LECT" && p.kind == PrincipalKind::Role));
    assert!(all.iter().any(|p| p.name == "SYS" && p.system && p.superuser == Some(true)));
    assert_eq!(all.iter().find(|p| p.name == "DBINE_SEC").unwrap().can_login, Some(false));

    let g = s.grants("DBINE_ANA").await.unwrap();
    let upd = g.iter().find(|x| x.privilege == "UPDATE").unwrap_or_else(|| panic!("direct UPDATE: {g:?}"));
    assert_eq!((upd.object.as_deref(), upd.object_kind.as_deref(), upd.grantable, upd.via.as_deref()), (Some("DBINE_SEC.FACTURAS"), Some("table"), true, None));
    assert!(g.iter().any(|x| x.privilege == "CREATE VIEW" && x.object.is_none() && x.via.is_none()));
    assert!(g.iter().any(|x| x.privilege == "CREATE SESSION" && x.via.is_none()));
    let sel = g.iter().find(|x| x.privilege == "SELECT").expect("SELECT through the role");
    assert_eq!(sel.via.as_deref(), Some("DBINE_LECT"));

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["UPDATE".into()], object: Some(table), from: "DBINE_ANA".into() })).await;
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["CREATE VIEW".into()], object: None, from: "DBINE_ANA".into() })).await;
    let g = s.grants("DBINE_ANA").await.unwrap();
    assert!(g.iter().all(|x| x.privilege != "UPDATE" && x.privilege != "CREATE VIEW"), "{g:?}");
    run(&mut s, &script(SecurityAction::SetPassword { name: "DBINE_ANA".into(), password: "Otra_Pw_987".into() })).await;

    // The fallback without DBA_ views, as the new user itself.
    let mut own = cfg.clone();
    own.username = Some("DBINE_ANA".into());
    own.password = Some("Otra_Pw_987".into());
    let mut a = d.connect(&own, None).await.unwrap();
    let mine = a.principals().await.unwrap();
    let me = mine.iter().find(|p| p.name == "DBINE_ANA").expect("own user");
    assert_eq!(me.member_of, vec!["DBINE_LECT".to_string()]);
    assert!(me.details.iter().any(|(k, _)| k == "Nota"));
    let g = a.grants("DBINE_ANA").await.unwrap();
    assert!(g.iter().any(|x| x.privilege == "SELECT" && x.via.as_deref() == Some("DBINE_LECT")), "{g:?}");
    drop(a);

    run(&mut s, &script(SecurityAction::SetLogin { name: "DBINE_ANA".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "DBINE_ANA").unwrap().disabled, Some(true));
    run(&mut s, &script(SecurityAction::SetLogin { name: "DBINE_ANA".into(), enabled: true })).await;
    run(&mut s, &script(SecurityAction::RemoveMember { role: "DBINE_LECT".into(), member: "DBINE_ANA".into() })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "DBINE_ANA".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "DBINE_LECT".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "DBINE_ANA" && p.name != "DBINE_LECT"));
    run(&mut s, "DROP USER DBINE_SEC CASCADE").await;
}

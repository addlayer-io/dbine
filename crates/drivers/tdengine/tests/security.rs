//! Users and permissions against a real TDengine (see tests/integration.rs):
//! `DBINE_TEST_TDENGINE_URL=http://localhost:25641 cargo test -p dbine-driver-tdengine --test security -- --ignored`

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TDENGINE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    })
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

#[tokio::test]
#[ignore]
async fn users_and_privileges() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_tdengine::drivers().remove(0);
    assert!(d.security().is_some());
    let mut s = d.connect(&c, None).await.unwrap();
    try_run(&mut s, "DROP USER dbine_ana").await;
    run(&mut s, "DROP DATABASE IF EXISTS dbine_sec; CREATE DATABASE dbine_sec").await;
    run(&mut s, "CREATE STABLE dbine_sec.st (ts TIMESTAMP, v INT) TAGS (k INT); CREATE TABLE dbine_sec.t1 (ts TIMESTAMP, v INT)").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345x".into()) })).await;
    let db = ObjectRef { kind: "database".into(), schema: None, name: "dbine_sec".into() };
    let st = ObjectRef { kind: "supertable".into(), schema: Some("dbine_sec".into()), name: "st".into() };
    let t1 = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_sec".into()), name: "t1".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["READ".into()], object: Some(db.clone()), to: "dbine_ana".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["WRITE".into()], object: Some(st.clone()), to: "dbine_ana".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["WRITE".into()], object: Some(t1.clone()), to: "dbine_ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").unwrap_or_else(|| panic!("{all:?}"));
    assert_eq!((ana.kind, ana.superuser, ana.disabled, ana.system), (PrincipalKind::User, Some(false), Some(false), false));
    assert!(all.iter().any(|p| p.name == "root" && p.system && p.superuser == Some(true)));

    let g = s.grants("dbine_ana").await.unwrap();
    assert!(g.iter().any(|x| x.privilege == "READ" && x.object.as_deref() == Some("dbine_sec") && x.object_kind.as_deref() == Some("database")), "{g:?}");
    assert!(g.iter().any(|x| x.privilege == "WRITE" && x.object.as_deref() == Some("dbine_sec.st") && x.object_kind.as_deref() == Some("supertable")), "{g:?}");
    assert!(g.iter().any(|x| x.privilege == "WRITE" && x.object.as_deref() == Some("dbine_sec.t1") && x.object_kind.as_deref() == Some("table")), "{g:?}");

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["WRITE".into()], object: Some(t1), from: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["READ".into()], object: Some(db), from: "dbine_ana".into() })).await;
    let g = s.grants("dbine_ana").await.unwrap();
    assert!(g.iter().all(|x| x.object.as_deref() != Some("dbine_sec.t1") && x.object.as_deref() != Some("dbine_sec")), "{g:?}");

    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987".into() })).await;
    let mut as_ana = c.clone();
    as_ana.username = Some("dbine_ana".into());
    as_ana.password = Some("Otra_Pw_987".into());
    d.connect(&as_ana, None).await.expect("signs in with the new password");
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(true));
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: true })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana"));
    run(&mut s, "DROP DATABASE dbine_sec").await;
}

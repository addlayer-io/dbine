//! Users, roles and permissions against a real IoTDB (see tests/integration.rs):
//! `DBINE_TEST_IOTDB_URL=http://localhost:25405 cargo test -p dbine-driver-iotdb --test security -- --ignored`

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg(url: &str, user: &str, password: &str) -> ConnectionConfig {
    ConnectionConfig { driver: "iotdb".into(), host: url.into(), username: Some(user.into()), password: Some(password.into()), ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(sql, 100, &mut out).await;
}

#[tokio::test]
#[ignore]
async fn users_roles_and_privileges() {
    let url = std::env::var("DBINE_TEST_IOTDB_URL").expect("DBINE_TEST_IOTDB_URL");
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    assert!(d.security().is_some());
    let mut s = d.connect(&cfg(&url, "root", "root"), None).await.unwrap();
    try_run(&mut s, "DROP USER dbine_ana").await;
    try_run(&mut s, "DROP ROLE dbine_lect").await;
    try_run(&mut s, "DELETE DATABASE root.dbine_sec").await;
    run(&mut s, "CREATE DATABASE root.dbine_sec").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "role:dbine_lect".into(), member: "dbine_ana".into() })).await;
    let db = ObjectRef { kind: "database".into(), schema: None, name: "root.dbine_sec".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["READ_DATA".into()], object: Some(db.clone()), to: "role:dbine_lect".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["WRITE_DATA".into()], object: Some(db.clone()), to: "dbine_ana".into(), grantable: true })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["MANAGE_DATABASE".into()], object: None, to: "dbine_ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").unwrap_or_else(|| panic!("{all:?}"));
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["role:dbine_lect".to_string()]);
    assert!(all.iter().any(|p| p.name == "role:dbine_lect" && p.kind == PrincipalKind::Role), "{all:?}");
    assert!(all.iter().any(|p| p.name == "root" && p.system && p.superuser == Some(true)));

    let g = s.grants("dbine_ana").await.unwrap();
    let w = g.iter().find(|x| x.privilege == "WRITE_DATA").unwrap_or_else(|| panic!("{g:?}"));
    assert_eq!((w.object.as_deref(), w.grantable, w.via.as_deref()), (Some("root.dbine_sec.**"), true, None));
    let r = g.iter().find(|x| x.privilege == "READ_DATA").unwrap_or_else(|| panic!("{g:?}"));
    assert_eq!(r.via.as_deref(), Some("role:dbine_lect"));
    let m = g.iter().find(|x| x.privilege == "MANAGE_DATABASE").unwrap_or_else(|| panic!("{g:?}"));
    assert_eq!(m.object, None);
    let rg = s.grants("role:dbine_lect").await.unwrap();
    assert!(rg.iter().any(|x| x.privilege == "READ_DATA" && x.via.is_none()), "{rg:?}");

    // Revoke as the UI does: the grant's object comes back as a database.
    let listed = ObjectRef { kind: "database".into(), schema: None, name: w.object.clone().unwrap() };
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["WRITE_DATA".into()], object: Some(listed), from: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["MANAGE_DATABASE".into()], object: None, from: "dbine_ana".into() })).await;
    let g = s.grants("dbine_ana").await.unwrap();
    assert!(g.iter().all(|x| x.privilege != "WRITE_DATA" && x.privilege != "MANAGE_DATABASE"), "{g:?}");

    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987".into() })).await;
    d.connect(&cfg(&url, "dbine_ana", "Otra_Pw_987"), None).await.expect("signs in with the new password");
    run(&mut s, &script(SecurityAction::RemoveMember { role: "role:dbine_lect".into(), member: "dbine_ana".into() })).await;
    assert!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().member_of.is_empty());
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "role:dbine_lect".into(), kind: PrincipalKind::Role })).await;
    let all = s.principals().await.unwrap();
    assert!(all.iter().all(|p| p.name != "dbine_ana" && p.name != "role:dbine_lect"), "{all:?}");
    run(&mut s, "DELETE DATABASE root.dbine_sec").await;
}

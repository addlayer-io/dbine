//! Users, roles and permissions through the password test hook against a
//! plain PostgreSQL (DSQL has no emulator; see tests/integration.rs):
//!   DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql --test security -- --ignored
//! `sys.iam_pg_role_mappings` only exists in DSQL: here no role has IAM
//! mappings.

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[tokio::test]
#[ignore]
async fn users_roles_and_grants() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else { return };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("postgres".into()),
        ..Default::default()
    };
    let d = dbine_driver_dsql::drivers().pop().unwrap();
    assert!(d.security().is_some_and(|s| !s.passwords));
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    run(
        &mut s,
        "DROP TABLE IF EXISTS dq_sec; DROP SCHEMA IF EXISTS dq_s CASCADE;
         DROP ROLE IF EXISTS \"dq ana\"; DROP ROLE IF EXISTS dq_lect;
         CREATE SCHEMA dq_s; CREATE TABLE dq_s.dq_sec (id int PRIMARY KEY)",
    )
    .await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dq ana".into(), password: None })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dq_lect".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "dq_lect".into(), member: "dq ana".into() })).await;
    let table = ObjectRef { kind: "table".into(), schema: Some("dq_s".into()), name: "dq_sec".into() };
    let schema = ObjectRef { kind: "schema".into(), schema: None, name: "dq_s".into() };
    let grant = |p: &str, o: Option<ObjectRef>, to: &str, grantable| SecurityAction::Grant {
        privileges: vec![p.into()],
        object: o,
        to: to.into(),
        grantable,
    };
    run(&mut s, &script(grant("SELECT", Some(table.clone()), "dq_lect", false))).await;
    run(&mut s, &script(grant("USAGE", Some(schema), "dq_lect", false))).await;
    run(&mut s, &script(grant("UPDATE", Some(table.clone()), "dq ana", true))).await;
    run(&mut s, &script(grant("CONNECT", None, "dq ana", false))).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dq ana").unwrap();
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["dq_lect".to_string()]);
    assert_eq!(ana.disabled, Some(false));
    let lect = all.iter().find(|p| p.name == "dq_lect").unwrap();
    assert_eq!(lect.kind, PrincipalKind::Role);
    assert!(all.iter().any(|p| p.name == "postgres" && p.system && p.superuser == Some(true)));

    let g = s.grants("dq ana").await.unwrap();
    let has = |p: &str, o: Option<&str>, via: Option<&str>| {
        g.iter().any(|x| x.privilege == p && x.object.as_deref() == o && x.via.as_deref() == via)
    };
    assert!(has("UPDATE", Some("dq_s.dq_sec"), None), "{g:?}");
    assert!(g.iter().any(|x| x.privilege == "UPDATE" && x.grantable));
    assert!(has("CONNECT", Some("postgres"), None), "{g:?}");
    assert!(has("SELECT", Some("dq_s.dq_sec"), Some("dq_lect")), "{g:?}");
    assert!(has("USAGE", Some("dq_s"), Some("dq_lect")), "{g:?}");

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["UPDATE".into()], object: Some(table), from: "dq ana".into() })).await;
    run(&mut s, &script(SecurityAction::RemoveMember { role: "dq_lect".into(), member: "dq ana".into() })).await;
    run(&mut s, &script(SecurityAction::SetLogin { name: "dq ana".into(), enabled: false })).await;
    let g = s.grants("dq ana").await.unwrap();
    assert!(g.iter().all(|x| x.via.is_none() && x.privilege != "UPDATE"), "{g:?}");
    let ana = s.principals().await.unwrap().into_iter().find(|p| p.name == "dq ana").unwrap();
    assert_eq!(ana.can_login, Some(false));
    assert!(ana.member_of.is_empty());

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["CONNECT".into()], object: None, from: "dq ana".into() })).await;
    run(&mut s, "REVOKE ALL ON ALL TABLES IN SCHEMA dq_s FROM dq_lect; REVOKE ALL ON SCHEMA dq_s FROM dq_lect").await;
    run(&mut s, &script(SecurityAction::Drop { name: "dq ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dq_lect".into(), kind: PrincipalKind::Role })).await;
    assert!(!s.principals().await.unwrap().iter().any(|p| p.name.starts_with("dq")));
    run(&mut s, "DROP SCHEMA dq_s CASCADE").await;
}

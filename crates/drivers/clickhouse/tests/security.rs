//! Users, roles and permissions against a real server
//! (`DBINE_TEST_CLICKHOUSE_URL`, see tests/integration.rs; the container
//! needs `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1`):
//! `cargo test -p dbine-driver-clickhouse --test security -- --ignored`

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
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
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "DROP USER IF EXISTS dbine_ana; DROP ROLE IF EXISTS dbine_lect; DROP DATABASE IF EXISTS dbine_sec; CREATE DATABASE dbine_sec").await;
    run(&mut s, "CREATE TABLE dbine_sec.facturas (id Int32, total Decimal(10, 2)) ENGINE = MergeTree ORDER BY id").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "dbine_lect".into(), member: "dbine_ana".into() })).await;
    let table = ObjectRef { kind: "table".into(), schema: Some("dbine_sec".into()), name: "facturas".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: Some(table.clone()), to: "dbine_lect".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["INSERT".into()], object: Some(table.clone()), to: "dbine_ana".into(), grantable: true })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").unwrap_or_else(|| panic!("the user: {all:?}"));
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["dbine_lect".to_string()]);
    assert_eq!((ana.disabled, ana.system, ana.superuser), (Some(false), false, Some(false)));
    assert!(all.iter().any(|p| p.name == "dbine_lect" && p.kind == PrincipalKind::Role));
    // The container's admin is defined in users.xml.
    assert!(all.iter().any(|p| p.name == "dbine" && p.system && p.superuser == Some(true)), "{all:?}");

    let g = s.grants("dbine_ana").await.unwrap();
    let ins = g.iter().find(|x| x.privilege == "INSERT").unwrap_or_else(|| panic!("direct INSERT: {g:?}"));
    assert_eq!((ins.object.as_deref(), ins.object_kind.as_deref(), ins.grantable, ins.via.as_deref()), (Some("dbine_sec.facturas"), Some("table"), true, None));
    let sel = g.iter().find(|x| x.privilege == "SELECT").expect("SELECT through the role");
    assert_eq!(sel.via.as_deref(), Some("dbine_lect"));

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["INSERT".into()], object: Some(table), from: "dbine_ana".into() })).await;
    assert!(s.grants("dbine_ana").await.unwrap().iter().all(|x| x.privilege != "INSERT"));
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987!".into() })).await;
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(true));
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: true })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(false));
    run(&mut s, &script(SecurityAction::RemoveMember { role: "dbine_lect".into(), member: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_lect".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana" && p.name != "dbine_lect"));
    run(&mut s, "DROP DATABASE dbine_sec").await;
}

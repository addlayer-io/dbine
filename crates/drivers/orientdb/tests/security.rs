//! Users, roles and permissions against a real server (see
//! tests/integration.rs):
//! `DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 cargo test -p dbine-driver-orientdb --test security -- --ignored`.
//! Works on its own database, `dbine_sec`, created fresh.

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg(url: &str, user: Option<(&str, &str)>) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (u, p) = user.unwrap_or_else(|| auth.split_once(':').unwrap());
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(u.into()),
        password: Some(p.into()),
        database: "dbine_sec".into(),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

#[tokio::test]
#[ignore]
async fn users_roles_and_rules() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else { return };
    let d = dbine_driver_orientdb::drivers().pop().unwrap();
    let mut root = d.connect(&ConnectionConfig { database: String::new(), ..cfg(&url, None) }, None).await.unwrap();
    let _ = root.drop_database("dbine_sec").await;
    root.create_database("dbine_sec").await.unwrap();
    let mut s = d.connect(&cfg(&url, None), None).await.unwrap();
    run(&mut s, "CREATE CLASS Cliente EXTENDS V; INSERT INTO Cliente SET n = 1").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "ana b".into(), password: Some("p'w\\1".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "ventas".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "ventas".into(), member: "ana b".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "reader".into(), member: "ana b".into() })).await;
    let cliente = ObjectRef { kind: "vertex".into(), schema: None, name: "Cliente".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["UPDATE".into(), "DELETE".into()], object: Some(cliente.clone()), to: "ventas".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["READ".into()], object: None, to: "ventas".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "ana b").unwrap();
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.disabled, Some(false));
    let mut roles = ana.member_of.clone();
    roles.sort();
    assert_eq!(roles, ["reader", "ventas"]);
    assert!(all.iter().any(|p| p.name == "admin" && p.kind == PrincipalKind::Role && p.system && p.superuser == Some(true)));
    assert!(all.iter().any(|p| p.name == "ventas" && !p.system && p.superuser == Some(false)));

    let g = s.grants("ana b").await.unwrap();
    let has = |p: &str, o: Option<&str>, via: &str| g.iter().any(|x| x.privilege == p && x.object.as_deref() == o && x.via.as_deref() == Some(via));
    assert!(has("UPDATE", Some("Cliente"), "ventas") && has("DELETE", Some("Cliente"), "ventas"), "{g:?}");
    assert!(has("READ", None, "ventas"), "{g:?}");
    assert!(g.iter().any(|x| x.via.as_deref() == Some("reader")), "{g:?}");
    assert!(g.iter().any(|x| x.object_kind.as_deref() == Some("vertex")));
    let direct = s.grants("ventas").await.unwrap();
    assert!(direct.iter().all(|x| x.via.is_none()) && direct.len() == 3, "{direct:?}");

    // The password works, with its quote and backslash.
    let mut ana_s = d.connect(&cfg(&url, Some(("ana b", "p'w\\1"))), None).await.unwrap();
    run(&mut ana_s, "SELECT FROM Cliente").await;

    run(&mut s, &script(SecurityAction::SetLogin { name: "ana b".into(), enabled: false })).await;
    assert!(d.connect(&cfg(&url, Some(("ana b", "p'w\\1"))), None).await.is_err());
    run(&mut s, &script(SecurityAction::SetLogin { name: "ana b".into(), enabled: true })).await;
    run(&mut s, &script(SecurityAction::SetPassword { name: "ana b".into(), password: "nueva".into() })).await;
    d.connect(&cfg(&url, Some(("ana b", "nueva"))), None).await.unwrap();

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["DELETE".into()], object: Some(cliente), from: "ventas".into() })).await;
    run(&mut s, &script(SecurityAction::RemoveMember { role: "reader".into(), member: "ana b".into() })).await;
    let g = s.grants("ana b").await.unwrap();
    assert!(g.iter().all(|x| x.via.as_deref() == Some("ventas") && x.privilege != "DELETE"), "{g:?}");
    let ana = s.principals().await.unwrap().into_iter().find(|p| p.name == "ana b").unwrap();
    assert_eq!(ana.member_of, ["ventas"]);

    run(&mut s, &script(SecurityAction::Drop { name: "ana b".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "ventas".into(), kind: PrincipalKind::Role })).await;
    assert!(!s.principals().await.unwrap().iter().any(|p| p.name == "ana b" || p.name == "ventas"));
    drop(s);
    root.drop_database("dbine_sec").await.unwrap();
}

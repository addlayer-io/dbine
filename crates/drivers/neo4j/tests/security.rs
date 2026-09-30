//! Users, roles and permissions against real servers (URLs as in
//! tests/integration.rs, `user:password@host:port`):
//! `docker run -d --name dbine-test-neo4j-auth -p 27120:7687 -e NEO4J_ACCEPT_LICENSE_AGREEMENT=eval -e NEO4J_AUTH=neo4j/DbineAuth123 neo4j:5-enterprise`
//! `docker run -d --name dbine-test-neo4j-auth-ce -p 27122:7687 -e NEO4J_AUTH=neo4j/DbineAuth123 neo4j:5`
//! `docker run -d --name dbine-test-memgraph-auth -p 27123:7687 memgraph/memgraph`
//! then `DBINE_TEST_NEO4J_EE_URL=neo4j:DbineAuth123@localhost:27120 DBINE_TEST_NEO4J_CE_URL=neo4j:DbineAuth123@localhost:27122
//! DBINE_TEST_MEMGRAPH_URL=localhost:27123 cargo test -p dbine-driver-neo4j --test security -- --ignored`

use dbine_driver::{ConnectionConfig, Error, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

fn driver(id: &str) -> std::sync::Arc<dyn dbine_driver::Driver> {
    dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

async fn try_run(s: &mut Box<dyn Session>, text: &str) -> Result<(), Error> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_enterprise() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_EE_URL") else { return };
    let d = driver("neo4j");
    // As the UI does: a session on a user database, not `system`.
    let mut s = d.connect(&cfg("neo4j", &url), Some("neo4j")).await.unwrap();
    for q in ["DROP USER dbine_ana IF EXISTS", "DROP ROLE dbine_lect IF EXISTS"] {
        run(&mut s, q).await;
    }
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!x'".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "dbine_lect".into(), member: "dbine_ana".into() })).await;
    let grant = |p: &[&str], object: Option<ObjectRef>| SecurityAction::Grant {
        privileges: p.iter().map(|x| x.to_string()).collect(),
        object,
        to: "dbine_lect".into(),
        grantable: false,
    };
    run(&mut s, &script(grant(&["ACCESS", "READ", "USER MANAGEMENT"], None))).await;
    let person = ObjectRef { kind: "label".into(), schema: None, name: "Person".into() };
    run(&mut s, &script(grant(&["TRAVERSE", "SET PROPERTY {name}"], Some(person.clone())))).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!((ana.kind, ana.member_of.clone(), ana.superuser, ana.disabled), (PrincipalKind::User, vec!["dbine_lect".to_string()], Some(false), Some(false)));
    assert!(all.iter().any(|p| p.name == "dbine_lect" && p.kind == PrincipalKind::Role && !p.system));
    assert!(all.iter().any(|p| p.name == "admin" && p.system && p.superuser == Some(true)));
    assert!(all.iter().any(|p| p.name == "neo4j" && p.superuser == Some(true)));

    let g = s.grants("dbine_lect").await.unwrap();
    println!("{g:#?}");
    let find = |p: &str| g.iter().find(|x| x.privilege == p).unwrap_or_else(|| panic!("{p} in {g:?}")).clone();
    assert_eq!(find("READ {*}").object, None);
    assert_eq!(find("USER MANAGEMENT").object_kind.as_deref(), Some("dbms"));
    let t = find("TRAVERSE");
    assert_eq!((t.object.as_deref(), t.object_kind.as_deref()), (Some("Person"), Some("label")));
    assert!(g.iter().all(|x| x.via.is_none()));
    // A user's privileges come through its roles.
    let ug = s.grants("dbine_ana").await.unwrap();
    assert!(ug.iter().any(|x| x.privilege == "READ {*}" && x.via.as_deref() == Some("dbine_lect")), "{ug:?}");
    assert!(ug.iter().any(|x| x.via.as_deref() == Some("PUBLIC")));

    // Revoking what grants() returned round-trips.
    let revoke = |x: &dbine_driver::Grant| SecurityAction::Revoke {
        privileges: vec![x.privilege.clone()],
        object: x.object.as_ref().filter(|_| x.object_kind.as_deref() != Some("dbms")).map(|o| ObjectRef {
            kind: x.object_kind.clone().unwrap_or_default(),
            schema: None,
            name: o.clone(),
        }),
        from: "dbine_lect".into(),
    };
    for x in [find("TRAVERSE"), find("SET PROPERTY {name}"), find("USER MANAGEMENT")] {
        run(&mut s, &script(revoke(&x))).await;
    }
    let left: Vec<String> = s.grants("dbine_lect").await.unwrap().into_iter().map(|x| x.privilege).collect();
    assert!(!left.iter().any(|p| p == "TRAVERSE" || p.starts_with("SET PROPERTY") || p == "USER MANAGEMENT"), "{left:?}");

    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987!".into() })).await;
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(true));
    run(&mut s, &script(SecurityAction::RemoveMember { role: "dbine_lect".into(), member: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_lect".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana" && p.name != "dbine_lect"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_community() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_CE_URL") else { return };
    let d = driver("neo4j");
    let mut s = d.connect(&cfg("neo4j", &url), Some("neo4j")).await.unwrap();
    run(&mut s, "DROP USER dbine_ana IF EXISTS").await;
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!x".into()) })).await;
    let all = s.principals().await.unwrap();
    assert!(all.iter().all(|p| p.kind == PrincipalKind::User));
    assert!(all.iter().any(|p| p.name == "dbine_ana"));
    assert!(matches!(s.grants("dbine_ana").await, Err(Error::Unsupported(_))));
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987!".into() })).await;
    let e = try_run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await.unwrap_err();
    assert!(e.to_string().contains("Enterprise"), "{e}");
    let e = try_run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await.unwrap_err();
    assert!(e.to_string().contains("Enterprise"), "{e}");
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn memgraph_community() {
    let Ok(url) = std::env::var("DBINE_TEST_MEMGRAPH_URL") else { return };
    let d = driver("memgraph");
    let mut s = d.connect(&cfg("memgraph", &url), None).await.unwrap();
    let _ = try_run(&mut s, "DROP USER dbine_ana").await;
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("pw'1".into()) })).await;
    let all = s.principals().await.unwrap();
    assert!(all.iter().any(|p| p.name == "dbine_ana" && p.kind == PrincipalKind::User));
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "pw2".into() })).await;
    // Community: privileges and roles need a license.
    assert!(matches!(s.grants("dbine_ana").await, Err(Error::Unsupported(_))));
    let e = try_run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await.unwrap_err();
    assert!(e.to_string().contains("Enterprise"), "{e}");
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana"));
}

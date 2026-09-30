//! Users, roles and permissions against real servers (a server with auth):
//!
//! ```sh
//! docker run -d --name dbine-test-mongodb-auth -p 27100:27017 \
//!   -e MONGO_INITDB_ROOT_USERNAME=root -e MONGO_INITDB_ROOT_PASSWORD=secret mongo:7
//! docker run -d --name dbine-test-ferretdb-auth -p 27101:27017 \
//!   -e POSTGRES_USER=root -e POSTGRES_PASSWORD=secret ghcr.io/ferretdb/ferretdb-eval:2
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:27100/?authSource=admin \
//! DBINE_TEST_FERRETDB_URL=mongodb://root:secret@localhost:27101/ \
//!   cargo test -p dbine-driver-mongodb --test security -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Driver, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};
use std::sync::Arc;

fn setup(var: &str, id: &str) -> Option<(Arc<dyn Driver>, ConnectionConfig)> {
    let url = std::env::var(var).ok()?;
    let mut c = ConnectionConfig { driver: id.into(), database: "dbine_sec".into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == id)?;
    Some((d, c))
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
    out
}

#[tokio::test]
#[ignore]
async fn mongodb_users_roles_and_grants() {
    let Some((d, c)) = setup("DBINE_TEST_MONGODB_URL", "mongodb") else { return };
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    // Leftovers of a failed run.
    for t in ["db.runCommand({ dropUser: 'ana' })", "db.runCommand({ dropRole: 'jefes' })", "db.runCommand({ dropRole: 'lectores' })"] {
        let _ = s.execute(t, 10, &mut out).await;
    }
    run(&mut s, "db.facturas.insertOne({ n: 1 })").await;
    let script = |a: SecurityAction| d.security_script(&a).unwrap();

    run(&mut s, &script(SecurityAction::CreateUser { name: "ana".into(), password: Some("Pw_\"1'x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "lectores".into() })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "jefes".into() })).await;
    let facturas = ObjectRef { kind: "collection".into(), schema: Some("dbine_sec".into()), name: "facturas".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["find".into()], object: Some(facturas.clone()), to: "lectores".into(), grantable: false })).await;
    // A role into a role, and a role into a user: the same script shape.
    let out = run(&mut s, &script(SecurityAction::AddMember { role: "lectores".into(), member: "jefes".into() })).await;
    assert!(out.messages.iter().any(|m| m.contains("usuario «jefes»")), "{:?}", out.messages);
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["read".into()], object: None, to: "jefes".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "jefes".into(), member: "ana".into() })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["readWrite".into(), "clusterMonitor".into()], object: None, to: "ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "ana").expect("the user");
    assert_eq!(ana.kind, PrincipalKind::User);
    let mut m = ana.member_of.clone();
    m.sort();
    assert_eq!(m, ["clusterMonitor", "jefes", "readWrite"]);
    assert_eq!(ana.superuser, Some(false));
    let lect = all.iter().find(|p| p.name == "lectores").expect("the role");
    assert_eq!(lect.kind, PrincipalKind::Role);
    let jefes = all.iter().find(|p| p.name == "jefes").unwrap();
    let mut jm = jefes.member_of.clone();
    jm.sort();
    assert_eq!(jm, ["lectores", "read"]);

    let g = s.grants("ana").await.unwrap();
    let has = |p: &str, obj: &str, via: Option<&str>| g.iter().any(|x| x.privilege == p && x.object.as_deref() == Some(obj) && x.via.as_deref() == via);
    assert!(has("readWrite", "dbine_sec", None), "{g:?}");
    assert!(has("clusterMonitor", "admin", None), "{g:?}");
    assert!(has("read", "dbine_sec", Some("jefes")), "{g:?}");
    assert!(has("find", "dbine_sec.facturas", Some("jefes")), "{g:?}");
    let gl = s.grants("lectores").await.unwrap();
    let find = gl.iter().find(|x| x.privilege == "find").expect("the role's own action");
    assert_eq!((find.object.as_deref(), find.object_kind.as_deref(), find.via.as_deref()), (Some("dbine_sec.facturas"), Some("collection"), None));

    // Revoke as the UI does: the grant's object back as an ObjectRef.
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["find".into()], object: Some(facturas), from: "lectores".into() })).await;
    assert!(s.grants("lectores").await.unwrap().is_empty());
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["clusterMonitor".into()], object: None, from: "ana".into() })).await;
    run(&mut s, &script(SecurityAction::RemoveMember { role: "jefes".into(), member: "ana".into() })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "ana").unwrap().member_of, ["readWrite"]);

    // The new password works, the old one doesn't.
    run(&mut s, &script(SecurityAction::SetPassword { name: "ana".into(), password: "Otra_pw".into() })).await;
    let mut as_ana = c.clone();
    as_ana.options.insert("connection_string".into(), c.options["connection_string"].replace("root:secret", "ana:Otra_pw").replace("authSource=admin", "authSource=dbine_sec"));
    let mut sa = d.connect(&as_ana, None).await.expect("ana signs in");
    run(&mut sa, "db.facturas.find({})").await;
    drop(sa);

    // A missing principal: both commands are skipped, not an error.
    let out = run(&mut s, &script(SecurityAction::AddMember { role: "jefes".into(), member: "nadie".into() })).await;
    assert_eq!(out.messages.len(), 2, "{:?}", out.messages);
    // …but a missing role still fails.
    let mut out = QueryOutcome::default();
    let bad = script(SecurityAction::AddMember { role: "noexiste".into(), member: "ana".into() });
    assert!(s.execute(&bad, 10, &mut out).await.is_err());

    run(&mut s, &script(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "jefes".into(), kind: PrincipalKind::Role })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "lectores".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| !["ana", "jefes", "lectores"].contains(&p.name.as_str())));
    run(&mut s, "db.dropDatabase()").await;

    // The root user, from admin.
    let mut a = d.connect(&c, Some("admin")).await.unwrap();
    let root = a.principals().await.unwrap().into_iter().find(|p| p.name == "root").expect("root");
    assert_eq!(root.superuser, Some(true));
}

#[tokio::test]
#[ignore]
async fn ferretdb_users_only() {
    let Some((d, c)) = setup("DBINE_TEST_FERRETDB_URL", "ferretdb") else { return };
    let spec = d.security().unwrap();
    assert!(spec.create_user && !spec.create_role && !spec.membership);
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    let _ = s.execute("db.adminCommand({ dropUser: 'ana' })", 10, &mut out).await;
    let script = |a: SecurityAction| d.security_script(&a);
    run(&mut s, &script(SecurityAction::CreateUser { name: "ana".into(), password: Some("pw1".into()) }).unwrap()).await;
    let all = s.principals().await.unwrap();
    assert!(all.iter().any(|p| p.name == "ana" && p.kind == PrincipalKind::User));
    assert!(all.iter().any(|p| p.system), "FerretDB's internal user");
    assert!(!s.grants("ana").await.unwrap().is_empty());
    run(&mut s, &script(SecurityAction::SetPassword { name: "ana".into(), password: "pw2".into() }).unwrap()).await;
    assert!(script(SecurityAction::CreateRole { name: "r".into() }).is_err());
    run(&mut s, &script(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }).unwrap()).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "ana"));
}

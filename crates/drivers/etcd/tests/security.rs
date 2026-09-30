//! Users, roles and permissions against a real etcd (a throwaway one: the
//! test turns auth on and off again):
//!
//! ```sh
//! docker run -d --name dbine-test-etcd-auth -p 27113:2379 quay.io/coreos/etcd:v3.5.17 \
//!   etcd --advertise-client-urls http://0.0.0.0:2379 --listen-client-urls http://0.0.0.0:2379
//! DBINE_TEST_ETCD_AUTH_URL=http://localhost:27113 cargo test -p dbine-driver-etcd --test security -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg(user: Option<(&str, &str)>) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_ETCD_AUTH_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "etcd".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: user.map(|u| u.0.into()),
        password: user.map(|u| u.1.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    out
}

#[tokio::test]
#[ignore]
async fn users_roles_and_permissions() {
    let Some(c) = cfg(None) else { return };
    let d = dbine_driver_etcd::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    for cleanup in ["user delete dbine_ana", "role delete dbine_lect", "user delete root", "role delete root"] {
        let _ = s.execute(cleanup, 10, &mut QueryOutcome::default()).await;
    }
    let script = |a: SecurityAction| d.security_script(&a).unwrap();

    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("pw \"1\"".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "dbine_lect".into(), member: "dbine_ana".into() })).await;
    let prefix = ObjectRef { kind: "key".into(), schema: None, name: "/app/".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["READ".into()], object: Some(prefix), to: "dbine_lect".into(), grantable: false })).await;
    let key = ObjectRef { kind: "key".into(), schema: Some("/w/x".into()), name: "y".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["READWRITE".into()], object: Some(key), to: "dbine_lect".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!((ana.kind, ana.member_of.clone(), ana.superuser, ana.system), (PrincipalKind::User, vec!["dbine_lect".to_string()], Some(false), false));
    assert!(all.iter().any(|p| p.name == "dbine_lect" && p.kind == PrincipalKind::Role));

    let g = s.grants("dbine_ana").await.unwrap();
    let read = g.iter().find(|x| x.privilege == "READ").expect("READ through the role");
    assert_eq!((read.object.as_deref(), read.object_kind.as_deref(), read.via.as_deref()), (Some("/app/"), Some("prefix"), Some("dbine_lect")));
    let rw = g.iter().find(|x| x.privilege == "READWRITE").expect("READWRITE");
    assert_eq!((rw.object.as_deref(), rw.object_kind.as_deref()), (Some("/w/x.y"), Some("key")));
    let direct = s.grants("dbine_lect").await.unwrap();
    assert_eq!(direct.len(), 2);
    assert!(direct.iter().all(|x| x.via.is_none()));

    // Enforced once auth is on: the user reads /app/ but can't write it.
    run(&mut s, "put /app/a 1").await;
    run(&mut s, &script(SecurityAction::CreateUser { name: "root".into(), password: Some("rootpw".into()) })).await;
    // etcd may keep the root role around (it refuses to delete it).
    let _ = s.execute("role add root", 10, &mut QueryOutcome::default()).await;
    run(&mut s, "user grant-role root root").await;
    run(&mut s, "auth enable").await;
    {
        let mut ana = d.connect(&cfg(Some(("dbine_ana", "pw \"1\""))).unwrap(), None).await.unwrap();
        let o = run(&mut ana, "get /app/a").await;
        assert_eq!(o.results[0].rows.len(), 1);
        assert!(ana.execute("put /app/a 2", 10, &mut QueryOutcome::default()).await.is_err());
        assert!(ana.principals().await.is_err());
        let mut root = d.connect(&cfg(Some(("root", "rootpw"))).unwrap(), None).await.unwrap();
        let all = root.principals().await.unwrap();
        assert!(all.iter().any(|p| p.name == "root" && p.system && p.superuser == Some(true)));
        assert!(root.grants("root").await.unwrap().iter().any(|g| g.privilege == "READWRITE" && g.object.is_none()));
        run(&mut root, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "nueva".into() })).await;
        assert!(d.connect(&cfg(Some(("dbine_ana", "pw \"1\""))).unwrap(), None).await.is_err());
        d.connect(&cfg(Some(("dbine_ana", "nueva"))).unwrap(), None).await.unwrap();
        run(&mut root, "auth disable").await;
    }

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["READ".into()], object: Some(ObjectRef { kind: "prefix".into(), schema: None, name: "/app/".into() }), from: "dbine_lect".into() })).await;
    assert_eq!(s.grants("dbine_lect").await.unwrap().len(), 1);
    run(&mut s, &script(SecurityAction::RemoveMember { role: "dbine_lect".into(), member: "dbine_ana".into() })).await;
    assert!(s.grants("dbine_ana").await.unwrap().is_empty());
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_lect".into(), kind: PrincipalKind::Role })).await;
    run(&mut s, "user delete root\ndel /app/a").await;
    let _ = s.execute("role delete root", 10, &mut QueryOutcome::default()).await;
    assert!(!s.principals().await.unwrap().iter().any(|p| p.name.starts_with("dbine_")));
}

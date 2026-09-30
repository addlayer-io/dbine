//! ACL users and rules against a real server (see tests/integration.rs):
//! `DBINE_TEST_REDIS_URL=redis://localhost:25400 cargo test -p dbine-driver-redis --test security -- --ignored`
//! (`DBINE_TEST_VALKEY_URL` / `DBINE_TEST_DRAGONFLY_URL` for those servers).

use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome, SecurityAction, Session};

fn cfg(driver: &str, url: &str, user: Option<&str>, password: Option<&str>) -> ConnectionConfig {
    let rest = url.trim_start_matches("redis://");
    let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: user.map(Into::into),
        password: password.map(Into::into),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

async fn acl(driver: &str, url: &str) {
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let spec = d.security().expect("ACL support");
    assert!(spec.create_user && !spec.create_role && !spec.membership && !spec.per_database);
    let mut s = d.connect(&cfg(driver, url, None, None), None).await.unwrap();
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    let _ = s.execute("ACL DELUSER dbine_ana", 10, &mut QueryOutcome::default()).await;

    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("pw 1\"#".into()) })).await;
    let key = ObjectRef { kind: "key".into(), schema: None, name: "app:1".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["+@read".into(), "-@dangerous".into(), "+set".into()], object: None, to: "dbine_ana".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["~".into()], object: Some(key), to: "dbine_ana".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["&news*".into()], object: None, to: "dbine_ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!((ana.can_login, ana.disabled, ana.superuser, ana.system), (Some(true), Some(false), Some(false), false));
    let def = all.iter().find(|p| p.name == "default").expect("default");
    assert!(def.system);
    assert_eq!(def.superuser, Some(true));

    let g = s.grants("dbine_ana").await.unwrap();
    let has = |p: &str, o: Option<&str>| g.iter().any(|x| x.privilege == p && x.object.as_deref() == o);
    assert!(has("+@read", None) && has("+set", None), "{g:?}");
    assert!(g.iter().any(|x| x.privilege == "-@dangerous" && x.denied));
    assert!(has("~", Some("app:1")), "{g:?}");
    assert!(has("&", Some("news*")), "{g:?}");

    // The new user signs in with its password and reads its key only.
    run(&mut s, "SET app:1 hola").await;
    {
        let mut ana = d.connect(&cfg(driver, url, Some("dbine_ana"), Some("pw 1\"#")), None).await.unwrap();
        run(&mut ana, "GET app:1").await;
        assert!(ana.execute("GET otra", 10, &mut QueryOutcome::default()).await.is_err());
    }

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["+set".into()], object: None, from: "dbine_ana".into() })).await;
    let g = s.grants("dbine_ana").await.unwrap();
    assert!(g.iter().any(|x| x.privilege == "-set"), "{g:?}");

    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "nueva".into() })).await;
    assert!(matches!(d.connect(&cfg(driver, url, Some("dbine_ana"), Some("pw 1\"#")), None).await, Err(Error::AuthFailed(_))));
    d.connect(&cfg(driver, url, Some("dbine_ana"), Some("nueva")), None).await.unwrap();

    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    let all = s.principals().await.unwrap();
    assert_eq!(all.iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(true));
    assert!(d.connect(&cfg(driver, url, Some("dbine_ana"), Some("nueva")), None).await.is_err());

    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: dbine_driver::PrincipalKind::User })).await;
    assert!(!s.principals().await.unwrap().iter().any(|p| p.name == "dbine_ana"));
    run(&mut s, "DEL app:1").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn redis_acl() {
    if let Ok(url) = std::env::var("DBINE_TEST_REDIS_URL") {
        acl("redis", &url).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn valkey_acl() {
    if let Ok(url) = std::env::var("DBINE_TEST_VALKEY_URL") {
        acl("valkey", &url).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn dragonfly_acl() {
    if let Ok(url) = std::env::var("DBINE_TEST_DRAGONFLY_URL") {
        acl("dragonfly", &url).await;
    }
}

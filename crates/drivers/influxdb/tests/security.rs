//! Users and privileges against InfluxDB 1.8 with auth on (ignored by default):
//!
//! ```sh
//! docker run -d --name dbine-test-influxdb-auth -p 27142:8086 -e INFLUXDB_HTTP_AUTH_ENABLED=true \
//!   -e INFLUXDB_ADMIN_USER=admin -e INFLUXDB_ADMIN_PASSWORD=Dbine_pw1 influxdb:1.8
//! DBINE_TEST_INFLUXDB1_AUTH_URL=http://admin:Dbine_pw1@localhost:27142 \
//!   cargo test -p dbine-driver-influxdb --test security -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};
use std::sync::Arc;

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_INFLUXDB1_AUTH_URL").ok()?;
    let (scheme, rest) = url.split_once("://")?;
    let (auth, host) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    Some(ConnectionConfig {
        driver: "influxdb1".into(),
        host: format!("{scheme}://{host}"),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn users_and_privileges() {
    let Some(cfg) = cfg() else { return };
    let d = driver("influxdb1");
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP USER \"dbine_ana\"", 10, &mut out).await;
    run(&mut s, "CREATE DATABASE dbine_sec; CREATE DATABASE dbine_sec2").await;
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    let db = |n: &str| Some(ObjectRef { kind: "database".into(), schema: None, name: n.into() });

    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw'1\\x".into()) })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["READ".into()], object: db("dbine_sec"), to: "dbine_ana".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["READ".into(), "WRITE".into()], object: db("dbine_sec2"), to: "dbine_ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!((ana.kind, ana.superuser), (PrincipalKind::User, Some(false)));
    assert!(all.iter().any(|p| p.name == "admin" && p.superuser == Some(true)));
    let g = s.grants("dbine_ana").await.unwrap();
    let on = |db: &str| g.iter().find(|x| x.object.as_deref() == Some(db)).map(|x| x.privilege.clone());
    assert_eq!(on("dbine_sec").as_deref(), Some("READ"));
    assert_eq!(on("dbine_sec2").as_deref(), Some("ALL PRIVILEGES"));
    assert!(g.iter().all(|x| x.object_kind.as_deref() == Some("database")));

    // The password with a quote and a backslash signs in.
    let as_ana = ConnectionConfig { username: Some("dbine_ana".into()), password: Some("Pw'1\\x".into()), ..cfg.clone() };
    d.connect(&as_ana, Some("dbine_sec")).await.expect("the password works");
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw".into() })).await;
    assert!(d.connect(&as_ana, Some("dbine_sec")).await.is_err(), "the old password no longer works");

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["READ".into()], object: db("dbine_sec"), from: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["ALL".into()], object: None, to: "dbine_ana".into(), grantable: false })).await;
    let g = s.grants("dbine_ana").await.unwrap();
    assert!(g.iter().all(|x| x.object.as_deref() != Some("dbine_sec")), "{g:?}");
    assert!(g.iter().any(|x| x.privilege == "ALL PRIVILEGES" && x.object.is_none()));
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().superuser, Some(true));
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["ALL PRIVILEGES".into()], object: None, from: "dbine_ana".into() })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().superuser, Some(false));

    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana"));
    run(&mut s, "DROP DATABASE dbine_sec; DROP DATABASE dbine_sec2").await;
}

#[test]
fn v2_and_v3_use_tokens() {
    for id in ["influxdb", "influxdb3"] {
        let d = driver(id);
        assert!(d.security().is_none());
        assert!(matches!(d.security_script(&SecurityAction::CreateRole { name: "r".into() }), Err(Error::Unsupported(_))));
    }
    assert!(driver("influxdb1").security().is_some());
}

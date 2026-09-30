//! Users and permissions against a real Dremio OSS (see tests/integration.rs;
//! the first user must exist, which that test creates):
//! `DBINE_TEST_DREMIO_URL=http://localhost:25947 cargo test -p dbine-driver-dremio --test security -- --ignored`
//!
//! OSS has no roles or privileges: its users are listed as administrators,
//! grants answer "unsupported" and the server refuses the scripts.

use dbine_driver::{ConnectionConfig, Error, PrincipalKind, QueryOutcome, SecurityAction};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(std::env::var("DBINE_TEST_DREMIO_USER").unwrap_or_else(|_| "dbine".into())),
        password: Some(std::env::var("DBINE_TEST_DREMIO_PASSWORD").unwrap_or_else(|_| "secreto123".into())),
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn community_edition() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_dremio::drivers().remove(0);
    assert!(d.security().is_some());
    let mut s = d.connect(&c, None).await.unwrap();
    let all = s.principals().await.unwrap();
    let me = all.iter().find(|p| Some(&p.name) == c.username.as_ref()).unwrap_or_else(|| panic!("{all:?}"));
    assert_eq!((me.kind, me.superuser), (PrincipalKind::User, Some(true)));
    assert!(matches!(s.grants(&me.name).await, Err(Error::Unsupported(_))));

    let sql = d.security_script(&SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345x".into()) }).unwrap();
    let mut out = QueryOutcome::default();
    let e = s.execute(&sql, 10, &mut out).await.expect_err("OSS refuses CREATE USER");
    assert!(e.to_string().contains("Enterprise"), "{e}");
}

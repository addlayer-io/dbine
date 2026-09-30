//! Users and roles against a real Couchbase Server 8.0+ (SQL++ user
//! management), provisioned as in tests/integration.rs:
//!
//! ```sh
//! docker run -d --name dbine-test-couchbase -p 25891:8091 -p 25893:8093 couchbase/server:community
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test security -- --ignored
//! ```
//!
//! Community Edition has no groups and only the `admin`, `ro_admin` and
//! `bucket_full_access` roles, which is what this exercises.

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};
use std::time::Duration;

const USER: &str = "Administrator";
const PASS: &str = "secreto1";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
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
async fn users_and_roles() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_couchbase::drivers().remove(0);
    assert!(d.security().is_some());
    let mut admin = d.connect(&c, None).await.unwrap();
    let _ = admin.drop_database("dbine_sec").await;
    admin.create_database("dbine_sec").await.unwrap();
    // A session in the bucket: its query context must not change what the scripts name.
    let mut s = d.connect(&c, Some("dbine_sec")).await.unwrap();
    try_run(&mut s, "DROP USER dbine_ana").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345x".into()) })).await;
    let bucket = ObjectRef { kind: "database".into(), schema: None, name: "dbine_sec".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["bucket_full_access".into()], object: Some(bucket.clone()), to: "dbine_ana".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["ro_admin".into()], object: None, to: "dbine_ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").unwrap_or_else(|| panic!("{all:?}"));
    assert_eq!((ana.kind, ana.superuser, ana.system), (PrincipalKind::User, Some(false), false));
    assert!(all.iter().any(|p| p.name == USER && p.system && p.superuser == Some(true)), "{all:?}");

    let g = s.grants("dbine_ana").await.unwrap();
    let b = g.iter().find(|x| x.privilege == "bucket_full_access").unwrap_or_else(|| panic!("{g:?}"));
    assert_eq!((b.object.as_deref(), b.object_kind.as_deref(), b.via.as_deref()), (Some("dbine_sec"), Some("database"), None));
    assert!(g.iter().any(|x| x.privilege == "ro_admin" && x.object.is_none()), "{g:?}");

    // The new user reads the bucket with its password.
    let mut as_ana = c.clone();
    as_ana.username = Some("dbine_ana".into());
    as_ana.password = Some("Pw_12345x".into());
    d.connect(&as_ana, Some("dbine_sec")).await.expect("signs in");

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["bucket_full_access".into()], object: Some(bucket), from: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["ro_admin".into()], object: None, from: "dbine_ana".into() })).await;
    assert!(s.grants("dbine_ana").await.unwrap().is_empty());

    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987".into() })).await;
    as_ana.password = Some("Otra_Pw_987".into());
    tokio::time::sleep(Duration::from_millis(500)).await;
    d.connect(&as_ana, None).await.expect("signs in with the new password");
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana"));
    admin.drop_database("dbine_sec").await.unwrap();
}

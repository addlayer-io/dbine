//! Roles and permissions against a real Trino (`DBINE_TEST_TRINO_URL`, see
//! tests/integration.rs). The stock image has no access control that
//! manages them, so this checks that the tab says so and that the server
//! parses every script DBine writes (it refuses them for the catalog, not
//! for their syntax):
//! `cargo test -p dbine-driver-trino --test security -- --ignored`

use dbine_driver::{ConnectionConfig, Error, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TRINO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "trino".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn trino_security() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_trino::drivers().remove(0);
    assert!(d.security().is_some_and(|s| !s.create_user && s.create_role && s.per_database));
    let mut s = d.connect(&c, None).await.unwrap();

    let mut out = QueryOutcome::default();
    s.execute("DROP SCHEMA IF EXISTS dbine_sec CASCADE; CREATE SCHEMA dbine_sec; CREATE TABLE dbine_sec.t (a int)", 10, &mut out)
        .await
        .unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);

    // No roles or grants in the memory catalog: a clear message.
    match s.principals().await {
        Err(Error::Unsupported(m)) => assert!(m.contains("memory") && m.contains("no maneja roles"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(s.grants("dbine").await.unwrap().is_empty());

    let t = Some(ObjectRef { kind: "table".into(), schema: Some("dbine_sec".into()), name: "t".into() });
    let schema = Some(ObjectRef { kind: "schema".into(), schema: None, name: "dbine_sec".into() });
    for (action, refusal) in [
        (SecurityAction::Grant { privileges: vec!["SELECT".into()], object: t.clone(), to: "ana".into(), grantable: true }, "permission management"),
        (SecurityAction::Grant { privileges: vec!["INSERT".into()], object: schema.clone(), to: "lect IN memory".into(), grantable: false }, "permission management"),
        (SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: t.clone(), from: "ana".into() }, "permission management"),
        (SecurityAction::CreateRole { name: "lect IN memory".into() }, "role management"),
        (SecurityAction::CreateRole { name: "lect".into() }, "System roles are not enabled"),
        (SecurityAction::Drop { name: "lect IN memory".into(), kind: PrincipalKind::Role }, "role management"),
        (SecurityAction::AddMember { role: "lect IN memory".into(), member: "ana".into() }, "role management"),
        (SecurityAction::RemoveMember { role: "lect IN memory".into(), member: "jefes IN memory".into() }, "role management"),
    ] {
        let sql = d.security_script(&action).unwrap();
        let mut out = QueryOutcome::default();
        let err = match s.execute(&sql, 10, &mut out).await {
            Err(e) => e.to_string(),
            Ok(()) => out.error.clone().unwrap_or_default(),
        };
        assert!(err.contains(refusal), "{sql}: {err}");
    }
    assert!(matches!(d.security_script(&SecurityAction::CreateUser { name: "a".into(), password: Some("x".into()) }), Err(Error::Unsupported(_))));

    let mut out = QueryOutcome::default();
    s.execute("DROP SCHEMA dbine_sec CASCADE", 10, &mut out).await.unwrap();
}

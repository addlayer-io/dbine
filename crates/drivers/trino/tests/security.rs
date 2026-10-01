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

/// "Nuevo esquema…" / "Borrar esquema…" in the memory catalog: the schema
/// is created without owner, the grants and the owner change (`SET
/// AUTHORIZATION`, after them) parse but memory refuses them as a catalog
/// without permission management (no connector of the stock image keeps
/// owners or grants), the new empty schema is listed, RESTRICT refuses a
/// schema with a table and CASCADE drops it. Trino folds names to
/// lowercase, quoted or not.
#[tokio::test]
#[ignore]
async fn trino_schemas() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_trino::drivers().remove(0);
    let spec = d.schema_spec().unwrap();
    assert!(spec.owner && spec.cascade && !spec.privileges.is_empty());
    let mut s = d.connect(&c, None).await.unwrap();
    let run = async |s: &mut Box<dyn dbine_driver::Session>, sql: &str| -> std::result::Result<QueryOutcome, String> {
        let mut out = QueryOutcome::default();
        match s.execute(sql, 100, &mut out).await {
            Err(e) => Err(e.to_string()),
            Ok(()) => match out.error.clone() {
                Some(e) => Err(e),
                None => Ok(out),
            },
        }
    };
    let _ = run(&mut s, "DROP SCHEMA IF EXISTS \"dbine sch\" CASCADE").await;

    let owner = d.schema_owner_script(Some("memory"), "dbine sch", "ana").unwrap().expect("the owner goes after the grants");
    let create = d.create_schema_script(Some("memory"), "dbine sch", None).unwrap();
    run(&mut s, &create).await.unwrap();
    assert_eq!(create, "CREATE SCHEMA \"dbine sch\";");
    let show = run(&mut s, "SHOW CREATE SCHEMA \"dbine sch\"").await.unwrap();
    assert_eq!(show.results[0].rows[0][0].as_str(), Some("CREATE SCHEMA memory.\"dbine sch\""));

    for p in &spec.privileges {
        let g = d.schema_grant_script(Some("memory"), "dbine sch", &[p.to_string()], "ana", true).unwrap();
        let err = run(&mut s, &g).await.expect_err("memory has no grants");
        assert!(err.contains("permission management"), "{g}: {err}");
    }
    let err = run(&mut s, &owner).await.expect_err("memory keeps no owners");
    assert!(err.contains("permission management"), "{owner}: {err}");

    // Listed while empty; information_schema is the system one.
    let listed = s.list_schemas().await.unwrap().expect("Trino lists schemas");
    assert!(listed.iter().any(|x| x.name == "dbine sch" && !x.system), "{listed:?}");
    assert!(listed.iter().any(|x| x.name == "information_schema" && x.system), "{listed:?}");

    run(&mut s, "CREATE TABLE \"dbine sch\".t (a int)").await.unwrap();
    let err = run(&mut s, &d.drop_schema_script(None, "dbine sch", false).unwrap()).await.expect_err("RESTRICT keeps a non-empty schema");
    assert!(err.contains("non-empty"), "{err}");
    run(&mut s, &d.drop_schema_script(None, "dbine sch", true).unwrap()).await.unwrap();
    let left = run(&mut s, "SELECT count(*) FROM memory.information_schema.schemata WHERE schema_name = 'dbine sch'").await.unwrap();
    assert_eq!(left.results[0].rows[0][0], serde_json::json!(0));

    // An empty one drops without CASCADE.
    run(&mut s, &d.create_schema_script(None, "dbine_empty", None).unwrap()).await.unwrap();
    run(&mut s, &d.drop_schema_script(None, "dbine_empty", false).unwrap()).await.unwrap();
}

/// Presto's schemas: no owner, no grants, no CASCADE; an empty schema is
/// created and dropped (`DBINE_TEST_PRESTO_URL=http://localhost:25181`).
#[tokio::test]
#[ignore]
async fn presto_schemas() {
    let Ok(url) = std::env::var("DBINE_TEST_PRESTO_URL") else { return };
    let url = reqwest::Url::parse(&url).unwrap();
    let c = ConnectionConfig {
        driver: "presto".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    };
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == "presto").unwrap();
    assert_eq!(d.schema_spec(), Some(dbine_driver::SchemaSpec::default()));
    assert!(d.schema_owner_script(None, "x", "ana").is_err());
    assert!(d.drop_schema_script(None, "x", true).is_err());
    let mut s = d.connect(&c, None).await.unwrap();
    let mut run = async |sql: String| {
        let mut out = QueryOutcome::default();
        s.execute(&sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    };
    run(d.create_schema_script(None, "dbine_ps", None).unwrap()).await;
    run("SELECT schema_name FROM memory.information_schema.schemata WHERE schema_name = 'dbine_ps'".into()).await;
    let listed = s.list_schemas().await.unwrap().expect("Presto lists schemas");
    assert!(listed.iter().any(|x| x.name == "dbine_ps" && !x.system), "{listed:?}");
    assert!(listed.iter().any(|x| x.name == "information_schema" && x.system), "{listed:?}");
    let mut run = async |sql: String| {
        let mut out = QueryOutcome::default();
        s.execute(&sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    };
    run(d.drop_schema_script(None, "dbine_ps", false).unwrap()).await;
}

//! Fine-grained access control scripts against the Cloud Spanner emulator
//! (see tests/integration.rs):
//!   DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner --test security -- --ignored
//! The emulator runs the DDL but has no INFORMATION_SCHEMA views for roles
//! nor column-level grants: reading must say so instead of failing
//! obscurely.

use dbine_driver::{ConnectionConfig, Error, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction};
use serde_json::json;

#[tokio::test]
#[ignore]
async fn role_scripts_run() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http
        .post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": "CREATE DATABASE `dbsec`" }))
        .send()
        .await;
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "dbsec".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut run = async |sql: String| {
        let mut out = QueryOutcome::default();
        s.execute(&sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    };
    run("CREATE TABLE IF NOT EXISTS Facturas (id INT64, total FLOAT64) PRIMARY KEY (id)".into()).await;
    run("CREATE OR REPLACE VIEW VFact SQL SECURITY INVOKER AS SELECT f.id FROM Facturas f".into()).await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    let table = ObjectRef { kind: "table".into(), schema: None, name: "Facturas".into() };
    let view = ObjectRef { kind: "view".into(), schema: None, name: "VFact".into() };
    run(script(SecurityAction::CreateRole { name: "lectores".into() })).await;
    run(script(SecurityAction::CreateRole { name: "ventas".into() })).await;
    run(script(SecurityAction::Grant { privileges: vec!["SELECT".into(), "INSERT".into()], object: Some(table.clone()), to: "lectores".into(), grantable: false })).await;
    run(script(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: Some(view.clone()), to: "ventas".into(), grantable: false })).await;
    run(script(SecurityAction::AddMember { role: "lectores".into(), member: "ventas".into() })).await;
    run(script(SecurityAction::RemoveMember { role: "lectores".into(), member: "ventas".into() })).await;
    run(script(SecurityAction::Revoke { privileges: vec!["INSERT".into()], object: Some(table.clone()), from: "lectores".into() })).await;
    run(script(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: Some(table), from: "lectores".into() })).await;
    run(script(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: Some(view), from: "ventas".into() })).await;
    run(script(SecurityAction::Drop { name: "ventas".into(), kind: PrincipalKind::Role })).await;
    run(script(SecurityAction::Drop { name: "lectores".into(), kind: PrincipalKind::Role })).await;

    match s.principals().await {
        Ok(p) => assert!(p.iter().any(|p| p.name == "public" && p.system)),
        Err(e) => assert!(matches!(e, Error::Unsupported(_)), "{e}"),
    }
}

/// Named schemas on the emulator: created, listed while empty (and
/// INFORMATION_SCHEMA as a system one), refused to drop while they hold a
/// table (no CASCADE), then dropped. `GRANT USAGE ON SCHEMA` is production
/// Spanner's: the emulator refuses it, and the test checks that it's that
/// refusal (with `DBINE_TEST_SPANNER_SCHEMA_GRANTS=1`, against a real
/// instance, it must run).
#[tokio::test]
#[ignore]
async fn named_schemas() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http
        .post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": "CREATE DATABASE `dbsch`" }))
        .send()
        .await;
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "dbsch".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    let spec = d.schema_spec().unwrap();
    assert!(!spec.owner && !spec.cascade && spec.privileges == vec!["USAGE"]);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let listed = s.list_schemas().await.unwrap().expect("Spanner lists schemas");
    assert!(listed.iter().any(|x| x.name == "INFORMATION_SCHEMA" && x.system), "{listed:?}");
    assert!(listed.iter().all(|x| !x.name.is_empty()), "the default schema isn't a node: {listed:?}");
    // A second session lists while `run` holds the first.
    let mut s2 = d.connect(&cfg, None).await.unwrap();
    let mut run = async |sql: String| -> std::result::Result<QueryOutcome, String> {
        let mut out = QueryOutcome::default();
        match s.execute(&sql, 10, &mut out).await {
            Err(e) => Err(e.to_string()),
            Ok(()) => out.error.clone().map_or(Ok(out), Err),
        }
    };
    let _ = run("DROP TABLE `ventas`.`t`".into()).await;
    let _ = run("DROP SCHEMA `ventas`".into()).await;
    run(d.create_schema_script(None, "ventas", None).unwrap()).await.unwrap();
    let seen = run("SELECT COUNT(*) FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = 'ventas'".into()).await.unwrap();
    assert_eq!(seen.results[0].rows[0][0].to_string().trim_matches('"'), "1");
    let _ = run("CREATE ROLE `dbine_lect`".into()).await;
    let grant = run(d.schema_grant_script(None, "ventas", &["USAGE".into()], "dbine_lect", false).unwrap()).await;
    if std::env::var("DBINE_TEST_SPANNER_SCHEMA_GRANTS").is_ok() {
        grant.unwrap();
    } else {
        let e = grant.expect_err("the emulator doesn't take ON SCHEMA");
        assert!(e.contains("SCHEMA") || e.contains("Syntax error") || e.contains("syntax"), "{e}");
    }
    let _ = run("REVOKE USAGE ON SCHEMA `ventas` FROM ROLE `dbine_lect`".into()).await;
    let _ = run("DROP ROLE `dbine_lect`".into()).await;
    let listed = s2.list_schemas().await.unwrap().unwrap();
    assert!(listed.iter().any(|x| x.name == "ventas" && !x.system), "the new empty schema is listed: {listed:?}");
    run("CREATE TABLE `ventas`.`t` (a INT64) PRIMARY KEY (a)".into()).await.unwrap();
    assert!(run(d.drop_schema_script(None, "ventas", false).unwrap()).await.is_err(), "a schema with a table isn't dropped");
    run("DROP TABLE `ventas`.`t`".into()).await.unwrap();
    run(d.drop_schema_script(None, "ventas", false).unwrap()).await.unwrap();
    let seen = run("SELECT COUNT(*) FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = 'ventas'".into()).await.unwrap();
    assert_eq!(seen.results[0].rows[0][0].to_string().trim_matches('"'), "0");
}

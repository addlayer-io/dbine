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

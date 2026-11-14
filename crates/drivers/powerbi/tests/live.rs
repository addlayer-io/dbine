//! Live tests against a Power BI / Fabric tenant (spec §8.3). There is no
//! Power BI image: these need the test tenant of spec §9.2 and run only by
//! hand, with the owner's OK:
//!
//! ```text
//! DBINE_TEST_POWERBI_TENANT=…            # tenant GUID or domain
//! DBINE_TEST_POWERBI_CLIENT_ID=…         # service principal (member of both workspaces)
//! DBINE_TEST_POWERBI_CLIENT_SECRET=…
//! DBINE_TEST_POWERBI_WORKSPACE_PRO=…     # shared capacity
//! DBINE_TEST_POWERBI_WORKSPACE_FABRIC=…  # Fabric trial capacity
//! DBINE_TEST_POWERBI_MODEL=…             # the sample model, published to both
//! DBINE_TEST_POWERBI_VIEWER_TOKEN=…      # a token of a Read + Build only user
//! cargo test -p dbine-driver-powerbi --test live -- --ignored --test-threads=1
//! ```
//!
//! Each test puts back what it changes (a schedule is read first and
//! written back at the end, even on failure); refreshes can't be undone, so
//! they run on the test model only.

use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;

fn env(k: &str) -> String {
    std::env::var(format!("DBINE_TEST_POWERBI_{k}"))
        .unwrap_or_else(|_| panic!("DBINE_TEST_POWERBI_{k} is not set"))
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_powerbi::drivers().remove(0)
}

/// A service-principal connection to `workspace`.
fn sp(workspace: &str, read_only: bool) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: "powerbi".into(),
        read_only,
        ..Default::default()
    };
    for (k, v) in [
        ("auth_mode", "entra_sp".to_string()),
        ("tenant", env("TENANT")),
        ("client_id", env("CLIENT_ID")),
        ("client_secret", env("CLIENT_SECRET")),
        ("workspace", workspace.to_string()),
    ] {
        c.options.insert(k.into(), v);
    }
    c
}

async fn open(cfg: &ConnectionConfig) -> Box<dyn Session> {
    driver()
        .connect(cfg, Some(&env("MODEL")))
        .await
        .expect("connect")
}

async fn run(s: &mut dyn Session, dax: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(dax, 1000, &mut out).await.expect(dax);
    out
}

#[tokio::test]
#[ignore]
async fn lists_the_models_of_both_workspaces() {
    for w in [env("WORKSPACE_PRO"), env("WORKSPACE_FABRIC")] {
        let mut s = open(&sp(&w, true)).await;
        let models = s.list_databases().await.unwrap();
        assert!(models.contains(&env("MODEL")), "{w}: {models:?}");
        let v = s.server_version().await.unwrap();
        assert!(v.starts_with("Power BI"), "{v}");
    }
}

#[tokio::test]
#[ignore]
async fn json_on_pro_and_arrow_on_fabric() {
    let mut pro = open(&sp(&env("WORKSPACE_PRO"), true)).await;
    let out = run(pro.as_mut(), "EVALUATE { 1 }").await;
    assert_eq!(out.results.len(), 1);
    // COLUMNSTATISTICS runs on executeQueries for anyone with Build.
    let out = run(pro.as_mut(), "EVALUATE COLUMNSTATISTICS()").await;
    assert!(!out.results[0].rows.is_empty());
    let mut fabric = open(&sp(&env("WORKSPACE_FABRIC"), true)).await;
    // Several EVALUATEs in one DEFINE unit: one result each.
    let out = run(
        fabric.as_mut(),
        "DEFINE VAR x = 1\nEVALUATE { x }\nEVALUATE { x + 1 }",
    )
    .await;
    assert_eq!(out.results.len(), 2);
}

#[tokio::test]
#[ignore]
async fn explorer_and_definitions() {
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), true)).await;
    let objects = s.list_objects().await.unwrap();
    assert!(objects.iter().any(|o| o.kind == "table"));
    assert!(objects
        .iter()
        .any(|o| o.kind == "measure" && o.parent.is_some()));
    let schema = s.database_schema().await.unwrap();
    assert!(!schema.is_empty());
    let functions = s.function_catalog().await.unwrap();
    assert!(functions
        .iter()
        .any(|f| f.name.eq_ignore_ascii_case("SUMMARIZECOLUMNS")));
}

#[tokio::test]
#[ignore]
async fn a_viewer_reads_with_a_pasted_token() {
    let mut c = ConnectionConfig {
        driver: "powerbi".into(),
        read_only: true,
        ..Default::default()
    };
    c.options.insert("auth_mode".into(), "token".into());
    c.options.insert("access_token".into(), env("VIEWER_TOKEN"));
    c.options.insert("workspace".into(), env("WORKSPACE_PRO"));
    let mut s = open(&c).await;
    let out = run(s.as_mut(), "EVALUATE { 1 }").await;
    assert_eq!(out.results.len(), 1);
    // A Build-only viewer gets tables at least.
    let objects = s.list_objects().await.unwrap();
    assert!(objects.iter().any(|o| o.kind == "table"));
}

#[tokio::test]
#[ignore]
async fn read_only_connections_refuse_every_write() {
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), true)).await;
    let jobs = s.jobs().await.unwrap();
    let Some(job) = jobs.jobs.first() else {
        panic!("the test model has no refresh schedule");
    };
    assert!(s.run_job(&job.id).await.is_err());
    assert!(s.set_job_enabled(&job.id, false).await.is_err());
    let mut out = QueryOutcome::default();
    assert!(s
        .execute("GRANT Read TO \"nobody@example.com\"", 10, &mut out)
        .await
        .is_err());
}

#[tokio::test]
#[ignore]
async fn the_schedule_round_trips() {
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), false)).await;
    let jobs = s.jobs().await.unwrap();
    let job = jobs.jobs.first().expect("a refresh schedule").clone();
    let before = s.job_detail(&job.id).await.unwrap();
    let flipped = s.set_job_enabled(&job.id, !job.enabled).await;
    // Put it back whatever happened.
    s.set_job_enabled(&job.id, job.enabled).await.unwrap();
    flipped.unwrap();
    let after = s.job_detail(&job.id).await.unwrap();
    assert_eq!(before.job.enabled, after.job.enabled);
}

//! Live tests of the model side (part C: metadata tiers, definitions,
//! dependencies, functions, properties, health, monitor, security, refresh
//! and the model's administration) against a Power BI / Fabric tenant
//! (spec §8.3). There is no Power BI image: these need the test tenant of
//! spec §9.2 and run only by hand, with the owner's OK:
//!
//! ```text
//! DBINE_TEST_POWERBI_TENANT=…            # tenant GUID or domain
//! DBINE_TEST_POWERBI_CLIENT_ID=…         # service principal (member of both workspaces)
//! DBINE_TEST_POWERBI_CLIENT_SECRET=…
//! DBINE_TEST_POWERBI_WORKSPACE_PRO=…     # shared capacity
//! DBINE_TEST_POWERBI_WORKSPACE_FABRIC=…  # Fabric trial capacity
//! DBINE_TEST_POWERBI_MODEL=…             # the sample model, published to both
//! DBINE_TEST_POWERBI_VIEWER_TOKEN=…      # a token of a Read + Build only user
//! DBINE_TEST_POWERBI_GRANTEE=…           # a user to grant and revoke (UPN), not in the workspaces
//! cargo test -p dbine-driver-powerbi --test live_model -- --ignored --test-threads=1
//! ```
//!
//! Each test puts back what it changes (a schedule, a name, a description,
//! a grant), even when an assertion fails; refreshes can't be undone, so
//! they run on the test model only.

use dbine_driver::{
    ConnectionConfig, DependencyScan, DependencyTarget, Driver, FormValues, ObjectRef,
    QueryOutcome, Session,
};
use std::collections::BTreeMap;
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

fn viewer(workspace: &str) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: "powerbi".into(),
        read_only: true,
        ..Default::default()
    };
    c.options.insert("auth_mode".into(), "token".into());
    c.options.insert("access_token".into(), env("VIEWER_TOKEN"));
    c.options.insert("workspace".into(), workspace.into());
    c
}

async fn open(cfg: &ConnectionConfig) -> Box<dyn Session> {
    driver()
        .connect(cfg, Some(&env("MODEL")))
        .await
        .expect("connect")
}

async fn listing(cfg: &ConnectionConfig) -> Box<dyn Session> {
    driver().connect(cfg, None).await.expect("connect")
}

fn kinds(objects: &[dbine_driver::DbObject], kind: &str) -> usize {
    objects.iter().filter(|o| o.kind == kind).count()
}

#[tokio::test]
#[ignore]
async fn each_tier_reads_what_it_can() {
    // A Build-only viewer on Pro: tables and columns (COLUMNSTATISTICS).
    let mut s = open(&viewer(&env("WORKSPACE_PRO"))).await;
    let objects = s.list_objects().await.unwrap();
    assert!(kinds(&objects, "table") > 0);
    let p = s.database_properties(&env("MODEL")).await.unwrap();
    assert!(
        p.info.iter().any(|i| i.label == "Leídos con"),
        "{:?}",
        p.info
    );
    // The service principal (a member: write permission) on Pro: the
    // definition API reads measures and relationships too (D2).
    let mut s = open(&sp(&env("WORKSPACE_PRO"), false)).await;
    let objects = s.list_objects().await.unwrap();
    assert!(kinds(&objects, "measure") > 0, "{objects:?}");
    assert!(kinds(&objects, "relationship") > 0);
    // On Fabric: the INFO functions.
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), true)).await;
    let objects = s.list_objects().await.unwrap();
    assert!(kinds(&objects, "measure") > 0 && kinds(&objects, "hierarchy") > 0);
    let measure = objects.iter().find(|o| o.kind == "measure").unwrap();
    let def = s
        .definition(&ObjectRef {
            kind: "measure".into(),
            schema: None,
            name: measure.name.clone(),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(def.contains("MEASURE '"), "{def}");
    // Columns of a table, typed.
    let table = objects.iter().find(|o| o.kind == "table").unwrap();
    let cols = s
        .columns(&ObjectRef {
            kind: "table".into(),
            schema: None,
            name: table.name.clone(),
        })
        .await
        .unwrap();
    assert!(!cols.is_empty() && cols.iter().all(|c| c.data_type != "Unknown"));
    let schema = s.database_schema().await.unwrap();
    assert!(schema.iter().any(|t| !t.foreign_keys.is_empty()));
}

#[tokio::test]
#[ignore]
async fn catalog_features() {
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), true)).await;
    let f = s.function_catalog().await.unwrap();
    assert!(f
        .iter()
        .any(|x| x.name.eq_ignore_ascii_case("SUMMARIZECOLUMNS")));
    let objects = s.list_objects().await.unwrap();
    let measure = objects.iter().find(|o| o.kind == "measure").unwrap();
    let report = s
        .dependents(
            &DependencyTarget {
                object: ObjectRef {
                    kind: "measure".into(),
                    schema: None,
                    name: measure.name.clone(),
                },
                column: None,
            },
            &DependencyScan {
                source_kinds: vec![],
                dialect: dbine_driver_powerbi::script_dialect(),
                foreign_keys: true,
            },
        )
        .await
        .unwrap();
    println!("dependents of {}: {:?}", measure.name, report.items);
    let hits = s
        .search_code(&dbine_driver::search::CodeSearch {
            text: "CALCULATE".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .unwrap();
    assert!(hits.scanned > 0);
    assert!(!s.row_estimates().await.unwrap().is_empty());
    let checks = s.health_checks(&env("MODEL")).await.unwrap();
    assert!(!checks.is_empty());
    let snap = s.monitor().await.unwrap();
    assert!(snap.metrics.iter().any(|m| m.key == "dbine_queries"));
}

/// Learn describes `INFO.VIEW.MEASURES[State]` only as "such as valid or
/// error": prints the states the service spells, and checks that the
/// sample model (no broken measures) gets no "Medidas con errores", since
/// only the known error states count (`health::is_error_state`).
#[tokio::test]
#[ignore]
async fn measure_states_as_the_service_spells_them() {
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), true)).await;
    let mut out = QueryOutcome::default();
    s.execute(
        "EVALUATE DISTINCT(SELECTCOLUMNS(INFO.VIEW.MEASURES(), \"State\", [State]))",
        100,
        &mut out,
    )
    .await
    .unwrap();
    println!("measure states: {:?}", out.results[0].rows);
    let checks = s.health_checks(&env("MODEL")).await.unwrap();
    assert!(
        !checks.iter().any(|c| c.id == "measures_errors"),
        "{checks:?}"
    );
}

/// The monitor's rows by table come from the data segments only (the
/// `H$`/`R$`/`U$` storage tables have their own records): they match
/// `INFO.STORAGETABLES`' row counts (`row_estimates`).
#[tokio::test]
#[ignore]
async fn monitor_rows_match_the_storage_tables() {
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), true)).await;
    let estimates = s.row_estimates().await.unwrap();
    let snap = s.monitor().await.unwrap();
    let sizes = snap
        .tables
        .iter()
        .find(|t| t.key == "table_sizes")
        .expect("model admin on capacity: memory readable");
    for row in &sizes.rows {
        let (Some(table), Some(rows)) = (row[0].as_str(), row[1].as_u64()) else {
            continue;
        };
        if let Some(e) = estimates.iter().find(|e| e.object.name == table) {
            assert_eq!(rows, e.rows, "{table}");
        }
    }
}

#[tokio::test]
#[ignore]
async fn users_and_permissions() {
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), true)).await;
    let principals = s.principals().await.unwrap();
    assert!(!principals.is_empty());
    let p = s.permissions(Some(&env("MODEL"))).await.unwrap();
    assert!(p.drop_database.is_denied(), "read-only connection");
    // Grant and revoke on a connection that may.
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), false)).await;
    let who = env("GRANTEE");
    let mut out = QueryOutcome::default();
    let granted = s
        .execute(&format!("GRANT Read, Build TO \"{who}\""), 10, &mut out)
        .await;
    let mut back = QueryOutcome::default();
    let revoked = s
        .execute(&format!("REVOKE ALL FROM \"{who}\""), 10, &mut back)
        .await;
    granted.unwrap();
    revoked.unwrap();
    let grants = s.grants(&who).await.unwrap();
    assert!(grants.iter().all(|g| g.object_kind.is_some()), "{grants:?}");
}

#[tokio::test]
#[ignore]
async fn the_schedule_form_saves_and_puts_back() {
    let mut s = listing(&sp(&env("WORKSPACE_FABRIC"), false)).await;
    let items = s.scheduler_items("refresh_schedule").await.unwrap();
    let item = items
        .items
        .iter()
        .find(|i| i.name == env("MODEL"))
        .expect("the test model has a refresh schedule")
        .clone();
    let form = s
        .scheduler_form("refresh_schedule", Some(&item.id))
        .await
        .unwrap();
    let before = FormValues {
        values: form.values.clone(),
        lists: BTreeMap::new(),
    };
    let mut changed = before.clone();
    changed
        .values
        .insert("times".into(), serde_json::json!("06:00\n18:30"));
    changed
        .values
        .insert("enabled".into(), serde_json::json!(true));
    let script = s
        .scheduler_script("refresh_schedule", Some(&item.id), &changed)
        .await
        .unwrap();
    assert!(script.starts_with("// ⚠"), "{script}");
    let saved = s
        .scheduler_save("refresh_schedule", Some(&item.id), &changed)
        .await;
    // Put it back whatever happened.
    s.scheduler_save("refresh_schedule", Some(&item.id), &before)
        .await
        .unwrap();
    saved.unwrap();
    let runs = s.job_runs(&item.id, 5).await.unwrap();
    println!("last refreshes: {runs:?}");
}

#[tokio::test]
#[ignore]
async fn refresh_now_and_cancel_on_capacity() {
    let mut s = listing(&sp(&env("WORKSPACE_FABRIC"), false)).await;
    let jobs = s.jobs().await.unwrap();
    let job = jobs
        .jobs
        .iter()
        .find(|j| j.name == env("MODEL"))
        .expect("the test model")
        .clone();
    let values = FormValues {
        values: BTreeMap::from([("type".to_string(), serde_json::json!("Calculate"))]),
        lists: BTreeMap::new(),
    };
    s.scheduler_action("refresh_schedule", &job.id, "run", &values)
        .await
        .unwrap();
    // An enhanced refresh can be cancelled while it runs.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let detail = s.job_detail(&job.id).await.unwrap();
    if detail.job.running {
        s.stop_job(&job.id).await.unwrap();
    }
    // A second refresh while one runs is refused.
    let _ = s
        .scheduler_action("refresh_schedule", &job.id, "run", &values)
        .await;
}

#[tokio::test]
#[ignore]
async fn rename_and_describe_put_back() {
    let model = env("MODEL");
    let mut s = listing(&sp(&env("WORKSPACE_FABRIC"), false)).await;
    let props = s.database_properties(&model).await.unwrap();
    let old = props.values.get("description").cloned().unwrap_or_default();
    let changes = BTreeMap::from([("description".to_string(), "DBine live test".to_string())]);
    let changed = s.alter_database(&model, &changes).await;
    s.alter_database(&model, &BTreeMap::from([("description".to_string(), old)]))
        .await
        .unwrap();
    changed.unwrap();
    let tmp = format!("{model} (dbine)");
    let d = driver();
    let script = d.rename_database_script(&model, &tmp, &[]).unwrap();
    let mut out = QueryOutcome::default();
    let renamed = s.execute(&script.statements[0], 10, &mut out).await;
    let back = d.rename_database_script(&tmp, &model, &[]).unwrap();
    let mut out2 = QueryOutcome::default();
    if renamed.is_ok() {
        s.list_databases().await.unwrap();
        s.execute(&back.statements[0], 10, &mut out2).await.unwrap();
    }
    renamed.unwrap();
}

#[tokio::test]
#[ignore]
async fn read_only_connections_refuse_the_administration() {
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), true)).await;
    let mut out = QueryOutcome::default();
    for unit in [
        "GRANT Read TO \"nobody@example.com\"",
        "RENAME MODEL \"x\" TO \"y\"",
    ] {
        assert!(s.execute(unit, 10, &mut out).await.is_err(), "{unit}");
    }
    assert!(s.drop_database(&env("MODEL")).await.is_err());
}

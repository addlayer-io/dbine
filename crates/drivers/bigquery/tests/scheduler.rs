//! BigQuery's scheduled queries in the scheduler designer.
//!
//! `emulator`: against the BigQuery emulator, which doesn't have the Data
//! Transfer Service: the scheduler answers no kinds with a note, the lists
//! are empty with it, every other call is refused, and ids are checked.
//! Skipped without `DBINE_TEST_BIGQUERY_URL`:
//!
//! ```sh
//! docker start dbine-test-bigquery
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//!   cargo test -p dbine-driver-bigquery --test scheduler -- --ignored --nocapture
//! ```
//!
//! `real_project`: against a real project with the Data Transfer API on and
//! credentials that can create datasets and transfer configs (a
//! service-account key file, or `adc`). It creates, edits part by part and
//! deletes scheduled queries in a dataset of its own; every object it makes
//! is named `dbine_test_sched_*` and is deleted at the end, whether the test
//! passed or not:
//!
//! ```sh
//! DBINE_TEST_BIGQUERY_PROJECT=mi-proyecto DBINE_TEST_BIGQUERY_CREDENTIALS=/ruta/clave.json \
//! DBINE_TEST_BIGQUERY_LOCATION=us \
//!   cargo test -p dbine-driver-bigquery --test scheduler real_project -- --ignored --nocapture
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, QueryOutcome, Session};
use serde_json::{json, Value};
use std::future::Future;
use std::time::Duration;

fn values_of(form: &ObjectForm) -> FormValues {
    FormValues {
        values: form.values.clone(),
        lists: Default::default(),
    }
}

fn unsupported<T: std::fmt::Debug>(r: dbine_driver::Result<T>) {
    assert!(
        matches!(&r, Err(Error::Unsupported(m)) if m.contains("emulador")),
        "{r:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn emulator() {
    let Ok(url) = std::env::var("DBINE_TEST_BIGQUERY_URL") else {
        eprintln!("DBINE_TEST_BIGQUERY_URL not set; skipping");
        return;
    };
    let mut c = ConnectionConfig {
        driver: "bigquery".into(),
        ..Default::default()
    };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    let d = dbine_driver_bigquery::drivers().pop().unwrap();
    assert!(d.capabilities().scheduler && d.capabilities().jobs);
    let mut s = d.connect(&c, Some("ds1")).await.unwrap();

    let info = s.scheduler_info().await.unwrap();
    eprintln!("{info:#?}");
    assert!(info.kinds.is_empty() && !info.logs);
    assert!(info.notes.iter().any(|n| n.contains("emulador")));
    let list = s.scheduler_items("job").await.unwrap();
    assert!(list.items.is_empty());
    assert!(list.notes.iter().any(|n| n.contains("emulador")));
    assert!(s.scheduler_items("schedule").await.is_err());

    let id = "projects/1/locations/us/transferConfigs/dbine_test_sched_x";
    let v = FormValues {
        values: [
            ("display_name".to_string(), json!("dbine_test_sched_x")),
            ("query".to_string(), json!("SELECT 1")),
        ]
        .into(),
        lists: Default::default(),
    };
    unsupported(s.scheduler_form("job", None).await);
    unsupported(s.scheduler_form("job", Some(id)).await);
    unsupported(s.scheduler_script("job", None, &v).await);
    unsupported(s.scheduler_save("job", None, &v).await);
    unsupported(s.scheduler_delete_script("job", id).await);
    unsupported(s.scheduler_delete("job", id).await);
    unsupported(s.clear_job_history(id).await);
    // One query, no steps; no scheduler log of its own.
    assert!(matches!(
        s.run_job_from(id, "x").await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        s.scheduler_logs().await,
        Err(Error::Unsupported(_))
    ));
    // Read-only: nothing changes, whatever the server is.
    let mut ro = ReadOnlySession::new(d.connect(&c, Some("ds1")).await.unwrap());
    for r in [
        ro.scheduler_save("job", None, &v).await.map(|_| ()),
        ro.scheduler_delete("job", id).await,
        ro.clear_job_history(id).await,
    ] {
        assert!(
            matches!(&r, Err(Error::Query(m)) if m.contains("solo lectura")),
            "{r:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// A real project
// ---------------------------------------------------------------------------

fn var(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_BIGQUERY_{k}"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn real_cfg() -> Option<(ConnectionConfig, String)> {
    let (project, key) = (var("PROJECT")?, var("CREDENTIALS")?);
    let location = var("LOCATION")
        .unwrap_or_else(|| "us".into())
        .to_ascii_lowercase();
    let mut cfg = ConnectionConfig {
        driver: "bigquery".into(),
        ..Default::default()
    };
    cfg.options.insert("project_id".into(), project);
    cfg.options.insert("location".into(), location.clone());
    if key == "adc" {
        cfg.options.insert("auth_mode".into(), "adc".into());
    } else {
        cfg.options.insert(
            "service_account_json".into(),
            std::fs::read_to_string(&key).unwrap(),
        );
    }
    Some((cfg, location))
}

async fn connect(cfg: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_bigquery::drivers()
        .pop()
        .unwrap()
        .connect(cfg, None)
        .await
        .expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let r = s.execute(sql, 100, &mut out).await;
    assert!(
        r.is_ok() && out.errors.is_empty(),
        "{sql}: {r:?} {:?}",
        out.errors
    );
}

/// Deletes every `dbine_test_sched_*` scheduled query and the test's
/// dataset.
async fn cleanup(cfg: &ConnectionConfig, dataset: &str) -> usize {
    let mut s = connect(cfg).await;
    let mut left = 0;
    if let Ok(list) = s.scheduler_items("job").await {
        for i in list
            .items
            .iter()
            .filter(|i| i.name.starts_with("dbine_test_sched_"))
        {
            if let Err(e) = s.scheduler_delete("job", &i.id).await {
                eprintln!("no se borró {}: {e}", i.id);
                left += 1;
            }
        }
    }
    let mut out = QueryOutcome::default();
    let _ = s
        .execute(
            &format!("DROP SCHEMA IF EXISTS `{dataset}` CASCADE"),
            10,
            &mut out,
        )
        .await;
    left
}

/// Runs the test, then the cleanup, and only then lets a failed assertion
/// through.
async fn with_cleanup(
    cfg: &ConnectionConfig,
    dataset: &str,
    body: impl Future<Output = ()> + Send + 'static,
) {
    let result = tokio::spawn(body).await;
    let left = cleanup(cfg, dataset).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
    assert_eq!(left, 0, "scheduled queries left behind");
}

/// Edits the form's values, shows the script, saves and reads it back.
async fn edit(s: &mut Box<dyn Session>, id: &str, changes: Value, mask: &str) -> ObjectForm {
    let form = s.scheduler_form("job", Some(id)).await.expect("form");
    let mut v = values_of(&form);
    for (k, x) in changes.as_object().unwrap() {
        v.values.insert(k.clone(), x.clone());
    }
    let script = s
        .scheduler_script("job", Some(id), &v)
        .await
        .expect("script");
    eprintln!("{script}");
    assert!(
        script.contains(&format!("updateMask={mask}")),
        "{mask}: {script}"
    );
    let saved = s.scheduler_save("job", Some(id), &v).await.expect("save");
    assert_eq!(saved.as_deref(), Some(id));
    let back = s.scheduler_form("job", Some(id)).await.expect("form");
    for (k, x) in changes.as_object().unwrap() {
        if k != "service_account" {
            assert_eq!(back.values.get(k), Some(x), "{k}");
        }
    }
    back
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn real_project() {
    let Some((cfg, location)) = real_cfg() else {
        eprintln!("sin DBINE_TEST_BIGQUERY_PROJECT / _CREDENTIALS: se omite");
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let ds = format!("dbine_test_sched_{stamp}");
    let name = format!("dbine_test_sched_{stamp}");
    let body = {
        let (cfg, ds, name, location) = (cfg.clone(), ds.clone(), name.clone(), location.clone());
        async move {
            let mut s = connect(&cfg).await;
            run(
                &mut s,
                &format!("CREATE SCHEMA `{ds}` OPTIONS(location = '{location}')"),
            )
            .await;
            let info = s.scheduler_info().await.unwrap();
            eprintln!("{info:#?}");
            let k = &info.kinds[0];
            assert!(k.key == "job" && k.can_create && k.can_edit && k.can_delete);

            // Create.
            let form = s.scheduler_form("job", None).await.unwrap();
            assert!(form.choices["dataset"].iter().any(|(d, _)| *d == ds));
            let tomorrow = (chrono::Utc::now() + chrono::Duration::days(1))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            let mut v = values_of(&form);
            for (k, x) in [
                ("display_name", json!(name)),
                ("query", json!("SELECT CURRENT_DATE() AS dia, 1 AS x")),
                ("dataset", json!(ds)),
                ("table", json!("dbine_test_sched_t")),
                ("write_disposition", json!("WRITE_TRUNCATE")),
                ("partitioning_field", json!("dia")),
                ("schedule", json!("every 24 hours")),
                ("start_time", json!(tomorrow)),
            ] {
                v.values.insert(k.into(), x);
            }
            let script = s.scheduler_script("job", None, &v).await.unwrap();
            eprintln!("{script}");
            assert!(
                script.contains("# POST https://bigquerydatatransfer.googleapis.com/v1/projects/")
            );
            let id = s
                .scheduler_save("job", None, &v)
                .await
                .unwrap()
                .expect("the new id");
            let list = s.scheduler_items("job").await.unwrap();
            assert!(list.items.iter().any(|i| i.id == id && i.name == name));
            let jobs = s.jobs().await.unwrap();
            assert!(
                jobs.jobs.iter().any(|j| j.id == id),
                "the tree's job opens it"
            );
            let f = s.scheduler_form("job", Some(&id)).await.unwrap();
            assert_eq!(f.values["dataset"], json!(ds));
            assert_eq!(f.values["partitioning_field"], json!("dia"));
            assert_eq!(f.values["location"], json!(location));

            // Edit each part, in place.
            edit(
                &mut s,
                &id,
                json!({ "display_name": format!("{name}_2") }),
                "display_name",
            )
            .await;
            edit(
                &mut s,
                &id,
                json!({ "query": "SELECT CURRENT_DATE() AS dia, 2 AS x" }),
                "params",
            )
            .await;
            edit(
                &mut s,
                &id,
                json!({ "write_disposition": "WRITE_APPEND" }),
                "params",
            )
            .await;
            edit(
                &mut s,
                &id,
                json!({ "schedule": "every 12 hours" }),
                "schedule",
            )
            .await;
            let later = (chrono::Utc::now() + chrono::Duration::days(30))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            edit(
                &mut s,
                &id,
                json!({ "end_time": later }),
                "schedule_options",
            )
            .await;
            edit(&mut s, &id, json!({ "mode": "manual" }), "schedule_options").await;
            edit(
                &mut s,
                &id,
                json!({ "mode": "schedule" }),
                "schedule_options",
            )
            .await;
            edit(&mut s, &id, json!({ "enabled": false }), "disabled").await;
            edit(
                &mut s,
                &id,
                json!({ "email_on_failure": true }),
                "email_preferences",
            )
            .await;
            let same = s.scheduler_form("job", Some(&id)).await.unwrap();
            let script = s
                .scheduler_script("job", Some(&id), &values_of(&same))
                .await
                .unwrap();
            assert!(script.contains("no hay cambios"), "{script}");

            // Run it, then clear its history.
            edit(&mut s, &id, json!({ "enabled": true }), "disabled").await;
            s.run_job(&id).await.unwrap();
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let runs = s.job_runs(&id, 5).await.unwrap();
                if runs
                    .first()
                    .is_some_and(|r| r.outcome != dbine_driver::JobOutcome::Running)
                {
                    break;
                }
            }
            s.clear_job_history(&id).await.unwrap();
            assert!(s.job_runs(&id, 5).await.unwrap().is_empty());

            // A DML one: no destination.
            let mut d = values_of(&s.scheduler_form("job", None).await.unwrap());
            for (k, x) in [
                ("display_name", json!(format!("{name}_dml"))),
                (
                    "query",
                    json!(format!("DELETE FROM `{ds}.dbine_test_sched_t` WHERE x = 0")),
                ),
                ("dataset", Value::Null),
                ("table", json!("")),
                ("location", json!(location)),
                ("mode", json!("manual")),
            ] {
                d.values.insert(k.into(), x);
            }
            let dml = s.scheduler_save("job", None, &d).await.unwrap().unwrap();
            let f = s.scheduler_form("job", Some(&dml)).await.unwrap();
            assert_eq!(f.values["dataset"], Value::Null);
            assert_eq!(f.values["mode"], json!("manual"));

            // Delete.
            let script = s.scheduler_delete_script("job", &dml).await.unwrap();
            assert!(script.contains("# DELETE "));
            s.scheduler_delete("job", &dml).await.unwrap();
            s.scheduler_delete("job", &id).await.unwrap();
            let list = s.scheduler_items("job").await.unwrap();
            assert!(!list.items.iter().any(|i| i.id == id || i.id == dml));
        }
    };
    with_cleanup(&cfg, &ds, body).await;
}

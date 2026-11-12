//! Spanner's backup schedules in the scheduler designer.
//!
//! `emulator`: against the Cloud Spanner emulator, which answers
//! UNIMPLEMENTED to every backup call: the scheduler answers no kinds with
//! a note, the list is empty with it, the other calls are refused as
//! unsupported, and ids are checked before anything reaches the API.
//!
//! ```sh
//! docker start dbine-test-spanner
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 \
//!   cargo test -p dbine-driver-spanner --test scheduler -- --ignored --nocapture
//! ```
//!
//! `real_instance`: against a real instance (credentials that can manage
//! backup schedules: roles/spanner.backupAdmin). It creates, edits part by
//! part, changes the backup type of and deletes a schedule named
//! `dbine_test_sched_*` on the database given, and deletes every such
//! schedule at the end, whether the test passed or not:
//!
//! ```sh
//! DBINE_TEST_SPANNER_PROJECT=mi-proyecto DBINE_TEST_SPANNER_INSTANCE=mi-instancia \
//! DBINE_TEST_SPANNER_DATABASE=mi-base DBINE_TEST_SPANNER_CREDENTIALS=/ruta/clave.json \
//!   cargo test -p dbine-driver-spanner --test scheduler real_instance -- --ignored --nocapture
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, Session};
use serde_json::{json, Value};
use std::future::Future;

fn values_of(form: &ObjectForm) -> FormValues {
    FormValues {
        values: form.values.clone(),
        lists: Default::default(),
    }
}

fn unsupported<T: std::fmt::Debug>(r: dbine_driver::Result<T>) {
    assert!(matches!(&r, Err(Error::Unsupported(_))), "{r:?}");
}

#[tokio::test]
#[ignore]
async fn emulator() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else {
        eprintln!("DBINE_TEST_SPANNER_URL not set; skipping");
        return;
    };
    // The instance and database the other tests use (made if missing).
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http
        .post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": "CREATE DATABASE `db1`" }))
        .send()
        .await;
    let mut cfg = ConnectionConfig {
        driver: "spanner".into(),
        database: "db1".into(),
        ..Default::default()
    };
    for (k, v) in [
        ("project_id", "test"),
        ("instance", "i1"),
        ("endpoint_url", url.as_str()),
    ] {
        cfg.options.insert(k.into(), v.into());
    }
    let driver = dbine_driver_spanner::drivers().pop().unwrap();
    assert!(driver.capabilities().scheduler && driver.capabilities().jobs);
    let mut s = driver.connect(&cfg, None).await.unwrap();

    let info = s.scheduler_info().await.unwrap();
    eprintln!("{info:#?}");
    assert!(info.kinds.is_empty() && !info.logs);
    assert!(info.notes.iter().any(|n| n.contains("emulador")));
    let list = s.scheduler_items("job").await.unwrap();
    assert!(list.items.is_empty());
    assert!(list.notes.iter().any(|n| n.contains("emulador")));

    let v = FormValues {
        values: [
            ("database".to_string(), json!("db1")),
            ("schedule_id".to_string(), json!("dbine_test_sched_x")),
            ("cron".to_string(), json!("0 2 * * *")),
            ("retention".to_string(), json!(86_400)),
        ]
        .into(),
        lists: Default::default(),
    };
    let id = "db1/dbine_test_sched_x";
    unsupported(s.scheduler_form("job", None).await);
    unsupported(s.scheduler_form("job", Some(id)).await);
    unsupported(s.scheduler_script("job", None, &v).await);
    unsupported(s.scheduler_save("job", None, &v).await);
    unsupported(s.scheduler_script("job", Some(id), &v).await);
    unsupported(s.scheduler_delete_script("job", id).await);
    unsupported(s.scheduler_delete("job", id).await);
    unsupported(s.clear_job_history(id).await);
    unsupported(s.run_job_from(id, "Backup").await);
    unsupported(s.scheduler_logs().await);

    // Ids and values from the app never leave the driver when they're bad.
    for bad in ["db1", "db1/../../backups", "db1/a b", "DB1/x", ""] {
        let e = s
            .scheduler_form("job", Some(bad))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("no es un id"), "{bad:?}: {e}");
        assert!(s.scheduler_delete("job", bad).await.is_err());
        assert!(s.scheduler_save("job", Some(bad), &v).await.is_err());
    }
    assert!(s.scheduler_items("backup").await.is_err());

    // Read-only: nothing changes.
    let mut ro = ReadOnlySession::new(driver.connect(&cfg, None).await.unwrap());
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
// A real instance
// ---------------------------------------------------------------------------

fn var(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_SPANNER_{k}"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn real_cfg() -> Option<ConnectionConfig> {
    let mut cfg = ConnectionConfig {
        driver: "spanner".into(),
        database: var("DATABASE")?,
        ..Default::default()
    };
    cfg.options.insert("project_id".into(), var("PROJECT")?);
    cfg.options.insert("instance".into(), var("INSTANCE")?);
    let key = var("CREDENTIALS")?;
    if key == "adc" {
        cfg.options.insert("auth_mode".into(), "adc".into());
    } else {
        cfg.options.insert(
            "service_account_json".into(),
            std::fs::read_to_string(&key).unwrap(),
        );
    }
    Some(cfg)
}

async fn connect(cfg: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_spanner::drivers()
        .pop()
        .unwrap()
        .connect(cfg, None)
        .await
        .expect("connect")
}

/// Deletes every `dbine_test_sched_*` schedule; how many were left.
async fn cleanup(cfg: &ConnectionConfig) -> usize {
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
    left
}

async fn with_cleanup(cfg: &ConnectionConfig, body: impl Future<Output = ()> + Send + 'static) {
    let result = tokio::spawn(body).await;
    let left = cleanup(cfg).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
    assert_eq!(left, 0, "backup schedules left behind");
}

/// Edits the form's values, shows the script, saves and reads it back.
async fn edit(s: &mut Box<dyn Session>, id: &str, changes: Value, expect: &str) -> ObjectForm {
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
    assert!(script.contains(expect), "{expect}: {script}");
    assert_eq!(
        s.scheduler_save("job", Some(id), &v)
            .await
            .expect("save")
            .as_deref(),
        Some(id)
    );
    let back = s.scheduler_form("job", Some(id)).await.expect("form");
    for (k, x) in changes.as_object().unwrap() {
        assert_eq!(back.values.get(k), Some(x), "{k}");
    }
    back
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn real_instance() {
    let Some(cfg) = real_cfg() else {
        eprintln!(
            "sin DBINE_TEST_SPANNER_PROJECT / _INSTANCE / _DATABASE / _CREDENTIALS: se omite"
        );
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let sid = format!("dbine_test_sched_{stamp}");
    let body = {
        let (cfg, sid) = (cfg.clone(), sid.clone());
        async move {
            let mut s = connect(&cfg).await;
            let info = s.scheduler_info().await.unwrap();
            eprintln!("{info:#?}");
            let k = &info.kinds[0];
            assert!(k.key == "job" && k.can_create && k.can_edit && k.can_delete);
            let standard = info.notes.iter().any(|n| n.contains("edición Standard"));

            // Create.
            let form = s.scheduler_form("job", None).await.unwrap();
            let mut v = values_of(&form);
            v.values.insert("schedule_id".into(), json!(sid));
            v.values.insert("retention".into(), json!(86_400));
            let script = s.scheduler_script("job", None, &v).await.unwrap();
            eprintln!("{script}");
            assert!(script.contains(&format!("backupScheduleId={sid}")));
            let id = s.scheduler_save("job", None, &v).await.unwrap().unwrap();
            assert_eq!(id, format!("{}/{sid}", cfg.database));
            let list = s.scheduler_items("job").await.unwrap();
            assert!(list.items.iter().any(|i| i.id == id));
            let jobs = s.jobs().await.unwrap();
            assert!(
                jobs.jobs.iter().any(|j| j.id == id),
                "the tree's job opens it"
            );

            // Edit each part in place.
            edit(
                &mut s,
                &id,
                json!({ "cron": "0 3 * * *" }),
                "updateMask=spec.cron_spec.text",
            )
            .await;
            edit(
                &mut s,
                &id,
                json!({ "retention": 172_800 }),
                "updateMask=retention_duration",
            )
            .await;
            edit(
                &mut s,
                &id,
                json!({ "encryption": "GOOGLE_DEFAULT_ENCRYPTION" }),
                "updateMask=encryption_config",
            )
            .await;
            let same = s.scheduler_form("job", Some(&id)).await.unwrap();
            let script = s
                .scheduler_script("job", Some(&id), &values_of(&same))
                .await
                .unwrap();
            assert!(script.contains("no hay cambios"), "{script}");

            // A new backup type: deleted and created again with the same id.
            let mut v = values_of(&same);
            v.values.insert("backup_type".into(), json!("incremental"));
            v.values.insert("cron".into(), json!("0 */4 * * *"));
            if standard {
                assert!(s.scheduler_save("job", Some(&id), &v).await.is_err());
            } else {
                let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
                assert!(
                    script.contains("# DELETE ") && script.contains("# POST "),
                    "{script}"
                );
                s.scheduler_save("job", Some(&id), &v).await.unwrap();
                let back = s.scheduler_form("job", Some(&id)).await.unwrap();
                assert_eq!(back.values["backup_type"], json!("incremental"));
            }

            // Delete.
            assert!(s
                .scheduler_delete_script("job", &id)
                .await
                .unwrap()
                .contains("# DELETE "));
            s.scheduler_delete("job", &id).await.unwrap();
            assert!(s.scheduler_form("job", Some(&id)).await.is_err());
        }
    };
    with_cleanup(&cfg, body).await;
}

//! Server jobs against a real workspace. Skipped without the variables (as
//! in tests/processes.rs):
//!
//! ```sh
//! DBINE_TEST_DATABRICKS_HOST=dbc-….cloud.databricks.com \
//! DBINE_TEST_DATABRICKS_WAREHOUSE=abcdef1234567890 \
//! DBINE_TEST_DATABRICKS_TOKEN=dapi… DBINE_TEST_DATABRICKS_CATALOG=main \
//!   cargo test -p dbine-driver-databricks --test jobs -- --ignored --nocapture
//! ```
//!
//! It creates its own job (two if/else condition tasks, which need no
//! compute) with a paused schedule, and deletes it at the end, also when an
//! assertion fails.

use dbine_driver::{ConnectionConfig, JobOutcome};
use serde_json::{json, Value as Json};
use std::time::Duration;

fn env(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_DATABRICKS_{k}"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn cfg() -> Option<ConnectionConfig> {
    let mut c = ConnectionConfig {
        driver: "databricks".into(),
        host: env("HOST")?,
        database: env("CATALOG")?,
        ..Default::default()
    };
    c.options.insert("warehouse".into(), env("WAREHOUSE")?);
    c.options.insert("token".into(), env("TOKEN")?);
    Some(c)
}

/// A call to the workspace's REST API with the test token.
async fn api(method: reqwest::Method, path: &str, body: Json) -> Json {
    let host = env("HOST").unwrap();
    let base = if host.starts_with("http") {
        host
    } else {
        format!("https://{host}")
    };
    let resp = reqwest::Client::new()
        .request(method, format!("{}{path}", base.trim_end_matches('/')))
        .bearer_auth(env("TOKEN").unwrap())
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "{path}: {status} {text}");
    serde_json::from_str(&text).unwrap_or(Json::Null)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn databricks_jobs() {
    let Some(cfg) = cfg() else {
        eprintln!("sin DBINE_TEST_DATABRICKS_*: se omite");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    assert!(d.capabilities().jobs);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let name = format!("dbine_test_jobs_{stamp}");
    let created = api(
        reqwest::Method::POST,
        "/api/2.1/jobs/create",
        json!({
            "name": name,
            "description": "dbine",
            "schedule": { "quartz_cron_expression": "0 0 3 * * ?", "timezone_id": "UTC", "pause_status": "PAUSED" },
            "tasks": [
                { "task_key": "dbine_test_jobs_a", "condition_task": { "op": "EQUAL_TO", "left": "1", "right": "1" } },
                { "task_key": "dbine_test_jobs_b", "depends_on": [{ "task_key": "dbine_test_jobs_a", "outcome": "true" }],
                  "condition_task": { "op": "GREATER_THAN", "left": "2", "right": "1" } }
            ]
        }),
    )
    .await;
    let job_id = created["job_id"].as_i64().expect("job_id").to_string();

    let body = {
        let (job_id, name) = (job_id.clone(), name.clone());
        tokio::spawn(async move {
            let mut s = d.connect(&cfg, None).await.expect("connect");

            let list = s.jobs().await.unwrap();
            eprintln!("{:?}", list.notes);
            let job = list.jobs.iter().find(|j| j.id == job_id).expect("listed");
            assert_eq!(job.name, name);
            assert_eq!(job.kind, "Job");
            assert_eq!(job.schedule.as_deref(), Some("0 0 3 * * ? (UTC)"));
            assert!(!job.enabled && job.can_run && job.can_enable && job.has_history);

            let detail = s.job_detail(&job_id).await.unwrap();
            let steps: Vec<(&str, Option<&str>)> = detail
                .steps
                .iter()
                .map(|s| (s.name.as_str(), s.kind.as_deref()))
                .collect();
            assert_eq!(
                steps,
                [
                    ("dbine_test_jobs_a", Some("If/else condition")),
                    ("dbine_test_jobs_b", Some("If/else condition"))
                ]
            );

            s.run_job(&job_id).await.unwrap();
            let mut done = None;
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let runs = s.job_runs(&job_id, 5).await.unwrap();
                if let Some(r) = runs
                    .first()
                    .filter(|r| !matches!(r.outcome, JobOutcome::Running | JobOutcome::Retrying))
                {
                    done = Some(r.clone());
                    break;
                }
            }
            let r = done.expect("the run ends");
            eprintln!("{r:#?}");
            assert_eq!(r.outcome, JobOutcome::Succeeded);
            assert_eq!(r.trigger.as_deref(), Some("manual"));
            assert_eq!(r.steps.len(), 2);
            assert!(r.started.as_deref().is_some_and(|t| t.ends_with('Z')));
            let after = s.job_detail(&job_id).await.unwrap().job;
            assert_eq!(after.last_outcome, Some(JobOutcome::Succeeded));

            s.set_job_enabled(&job_id, true).await.unwrap();
            assert!(s.job_detail(&job_id).await.unwrap().job.enabled);
            s.set_job_enabled(&job_id, false).await.unwrap();
            assert!(!s.job_detail(&job_id).await.unwrap().job.enabled);

            s.stop_job(&job_id).await.unwrap();

            let script: Json = serde_json::from_str(&s.job_script(&job_id).await.unwrap()).unwrap();
            assert_eq!(script["name"], json!(name));
            assert_eq!(script["tasks"].as_array().map(Vec::len), Some(2));

            for evil in [
                format!("{job_id}&job_id=1"),
                "../../2.0/sql/statements".into(),
                format!("-{job_id}"),
            ] {
                assert!(s.run_job(&evil).await.is_err(), "{evil}");
                assert!(s.set_job_enabled(&evil, true).await.is_err(), "{evil}");
                assert!(s.job_detail(&evil).await.is_err(), "{evil}");
            }
        })
    };
    let outcome = body.await;
    // Cleanup before reporting.
    api(
        reqwest::Method::POST,
        "/api/2.1/jobs/delete",
        json!({ "job_id": job_id.parse::<i64>().unwrap() }),
    )
    .await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

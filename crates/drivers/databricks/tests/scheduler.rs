//! The scheduler designer against a real workspace. Skipped without the
//! variables (as in tests/jobs.rs):
//!
//! ```sh
//! DBINE_TEST_DATABRICKS_HOST=dbc-….cloud.databricks.com \
//! DBINE_TEST_DATABRICKS_WAREHOUSE=abcdef1234567890 \
//! DBINE_TEST_DATABRICKS_TOKEN=dapi… DBINE_TEST_DATABRICKS_CATALOG=main \
//!   cargo test -p dbine-driver-databricks --test scheduler -- --ignored --nocapture
//! ```
//!
//! It creates its own job, `dbine_test_sched_<time>`, with if/else
//! condition tasks (they need no compute), runs it with another value for
//! its parameter and only one task, changes every part of it through the
//! designer, checks each change with `jobs/get`, runs it from a task,
//! clears its history and deletes it. Every job named `dbine_test_sched_*`
//! of this run is deleted at the end, also when an assertion fails.

use dbine_driver::{FormValues, JobOutcome, ObjectForm, Session};
use serde_json::{json, Value as Json};
use std::collections::BTreeMap;
use std::time::Duration;

fn env(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_DATABRICKS_{k}"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn cfg() -> Option<dbine_driver::ConnectionConfig> {
    let mut c = dbine_driver::ConnectionConfig {
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
async fn api(method: reqwest::Method, path: &str, body: Option<Json>) -> Json {
    let host = env("HOST").unwrap();
    let base = if host.starts_with("http") {
        host
    } else {
        format!("https://{host}")
    };
    let mut req = reqwest::Client::new()
        .request(method, format!("{}{path}", base.trim_end_matches('/')))
        .bearer_auth(env("TOKEN").unwrap());
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(status.is_success(), "{path}: {status} {text}");
    serde_json::from_str(&text).unwrap_or(Json::Null)
}

async fn settings(id: &str) -> Json {
    api(
        reqwest::Method::GET,
        &format!("/api/2.1/jobs/get?job_id={id}"),
        None,
    )
    .await["settings"]
        .clone()
}

/// What the app sends back for a form left as it came.
fn back(f: &ObjectForm) -> FormValues {
    FormValues {
        values: f.values.clone(),
        lists: f
            .sections
            .iter()
            .flat_map(|s| s.lists.iter())
            .map(|l| (l.key.clone(), l.rows.clone()))
            .collect(),
    }
}

fn row(v: Json) -> BTreeMap<String, Json> {
    serde_json::from_value(v).unwrap()
}

fn condition(key: &str, extra: Json) -> BTreeMap<String, Json> {
    let mut r = row(json!({
        "task_key": key, "type": "condition", "condition_op": "EQUAL_TO",
        "condition_left": "1", "condition_right": "1"
    }));
    for (k, v) in extra.as_object().unwrap() {
        r.insert(k.clone(), v.clone());
    }
    r
}

/// Deletes every job of this run (named `prefix…`).
async fn cleanup(prefix: &str) {
    let mut token: Option<String> = None;
    loop {
        let path = match &token {
            Some(t) => {
                let t: String = t
                    .bytes()
                    .map(|b| {
                        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                            (b as char).to_string()
                        } else {
                            format!("%{b:02X}")
                        }
                    })
                    .collect();
                format!("/api/2.1/jobs/list?limit=100&page_token={t}")
            }
            None => "/api/2.1/jobs/list?limit=100".into(),
        };
        let page = api(reqwest::Method::GET, &path, None).await;
        for j in page["jobs"].as_array().into_iter().flatten() {
            let name = j["settings"]["name"].as_str().unwrap_or_default();
            if name.starts_with(prefix) {
                api(
                    reqwest::Method::POST,
                    "/api/2.1/jobs/delete",
                    Some(json!({ "job_id": j["job_id"] })),
                )
                .await;
                eprintln!("deleted {name}");
            }
        }
        match page["next_page_token"].as_str() {
            Some(t) if page["has_more"] != json!(false) => token = Some(t.to_string()),
            _ => break,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn databricks_scheduler() {
    let Some(cfg) = cfg() else {
        eprintln!("sin DBINE_TEST_DATABRICKS_*: se omite");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    assert!(d.capabilities().scheduler);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let prefix = format!("dbine_test_sched_{stamp}");
    let body = {
        let prefix = prefix.clone();
        tokio::spawn(async move {
            let mut s: Box<dyn Session> = d.connect(&cfg, None).await.expect("connect");
            let info = s.scheduler_info().await.unwrap();
            eprintln!("{info:#?}");
            assert_eq!(info.kinds[0].key, "job");
            assert!(info.kinds[0].can_create);
            assert!(info.kinds[0].run_from_step && info.kinds[0].clear_history);
            assert!(info.kinds[0]
                .actions
                .iter()
                .any(|a| a.key == "run_with_params" && a.params));

            // Create: a paused schedule, three tasks, a parameter, tags.
            let mut v = back(&s.scheduler_form("job", None).await.unwrap());
            v.values.insert("name".into(), json!(prefix));
            v.values.insert("description".into(), json!("dbine"));
            v.values.insert("tags".into(), json!("dbine=test"));
            v.values.insert("trigger_mode".into(), json!("schedule"));
            v.values.insert("cron".into(), json!("0 0 3 * * ?"));
            v.values.insert("paused".into(), json!(true));
            v.lists.insert(
                "tasks".into(),
                vec![
                    condition("dbine_test_sched_a", json!({})),
                    condition(
                        "dbine_test_sched_b",
                        json!({ "depends_true": ["dbine_test_sched_a"] }),
                    ),
                    condition(
                        "dbine_test_sched_c",
                        json!({ "depends_on": ["dbine_test_sched_b"], "run_if": "ALL_DONE" }),
                    ),
                ],
            );
            v.lists.insert(
                "parameters".into(),
                vec![row(json!({ "name": "fecha", "default": "hoy" }))],
            );
            let script = s.scheduler_script("job", None, &v).await.unwrap();
            assert!(script.contains("-- POST /api/2.1/jobs/create"), "{script}");
            let id = s
                .scheduler_save("job", None, &v)
                .await
                .unwrap()
                .expect("id");
            let got = settings(&id).await;
            assert_eq!(got["name"], json!(prefix));
            assert_eq!(got["schedule"]["pause_status"], "PAUSED");
            assert_eq!(got["tasks"].as_array().unwrap().len(), 3);
            assert_eq!(
                got["parameters"],
                json!([{ "name": "fecha", "default": "hoy" }])
            );
            let items = s.scheduler_items("job").await.unwrap();
            // The login created it (IS_OWNER): changed and deleted as the
            // kind says.
            assert!(items.items.iter().any(|i| i.id == id
                && i.enabled == Some(false)
                && i.can_edit.is_none()
                && i.can_delete.is_none()));

            // An unchanged form changes nothing.
            let f = s.scheduler_form("job", Some(&id)).await.unwrap();
            assert!(!f.read_only, "{:?}", f.notes);
            let script = s
                .scheduler_script("job", Some(&id), &back(&f))
                .await
                .unwrap();
            assert!(script.contains("no hay cambios que guardar"), "{script}");

            // Run with other parameters: its job parameter as a field, only
            // its first task.
            let f = s
                .scheduler_action_form("job", &id, "run_with_params")
                .await
                .unwrap();
            assert_eq!(f.values["param:fecha"], json!("hoy"));
            let mut v = back(&f);
            v.values.insert("param:fecha".into(), json!("ayer"));
            v.values
                .insert("only".into(), json!(["dbine_test_sched_a"]));
            let script = s
                .scheduler_action_script("job", &id, "run_with_params", &v)
                .await
                .unwrap();
            assert!(script.contains("-- POST /api/2.1/jobs/run-now"), "{script}");
            s.scheduler_action("job", &id, "run_with_params", &v)
                .await
                .unwrap();
            let mut run = Json::Null;
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let list = api(
                    reqwest::Method::GET,
                    &format!("/api/2.1/jobs/runs/list?job_id={id}&limit=1"),
                    None,
                )
                .await;
                let r = &list["runs"][0];
                if r["state"]["life_cycle_state"] == "TERMINATED" {
                    run = api(
                        reqwest::Method::GET,
                        &format!("/api/2.1/jobs/runs/get?run_id={}", r["run_id"]),
                        None,
                    )
                    .await;
                    break;
                }
            }
            eprintln!("{run:#}");
            let fecha = run["job_parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|p| p["name"] == "fecha")
                .cloned()
                .unwrap_or(Json::Null);
            assert_eq!(fecha["value"], json!("ayer"), "{run:#}");
            let ran: Vec<&str> = run["tasks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["task_key"].as_str())
                .collect();
            assert_eq!(ran, ["dbine_test_sched_a"], "{run:#}");

            // Edit: rename a task (its dependents follow), drop one, add
            // one, resume and change the schedule, a health rule.
            let mut v = back(&f);
            let tasks = v.lists.get_mut("tasks").unwrap();
            tasks.retain(|r| r["task_key"] != "dbine_test_sched_c");
            tasks
                .iter_mut()
                .find(|r| r["task_key"] == "dbine_test_sched_a")
                .unwrap()
                .insert("task_key".into(), json!("dbine_test_sched_a2"));
            tasks.push(condition(
                "dbine_test_sched_d",
                json!({ "depends_false": ["dbine_test_sched_a"] }),
            ));
            v.values.insert("paused".into(), json!(false));
            v.values.insert("cron".into(), json!("0 30 4 * * ?"));
            v.lists.insert(
                "health".into(),
                vec![row(
                    json!({ "metric": "RUN_DURATION_SECONDS", "value": 3600 }),
                )],
            );
            let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
            assert!(script.contains("-- POST /api/2.1/jobs/update"), "{script}");
            assert!(script.contains("\"tasks/dbine_test_sched_c\""), "{script}");
            s.scheduler_save("job", Some(&id), &v).await.unwrap();
            let got = settings(&id).await;
            let mut keys: Vec<&str> = got["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["task_key"].as_str().unwrap())
                .collect();
            keys.sort();
            assert_eq!(
                keys,
                [
                    "dbine_test_sched_a2",
                    "dbine_test_sched_b",
                    "dbine_test_sched_d"
                ]
            );
            let b = got["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["task_key"] == "dbine_test_sched_b")
                .unwrap();
            assert_eq!(
                b["depends_on"],
                json!([{ "task_key": "dbine_test_sched_a2", "outcome": "true" }])
            );
            assert_eq!(got["schedule"]["quartz_cron_expression"], "0 30 4 * * ?");
            assert_ne!(got["schedule"]["pause_status"], "PAUSED");
            assert_eq!(got["health"]["rules"][0]["value"], 3600);

            // Parameters change by reset; the id and the rest stay.
            let f = s.scheduler_form("job", Some(&id)).await.unwrap();
            let mut v = back(&f);
            v.lists.insert("parameters".into(), vec![]);
            v.values.insert("trigger_mode".into(), json!("periodic"));
            v.values.insert("periodic_interval".into(), json!(2));
            v.values.insert("periodic_unit".into(), json!("DAYS"));
            let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
            assert!(script.contains("-- POST /api/2.1/jobs/reset"), "{script}");
            s.scheduler_save("job", Some(&id), &v).await.unwrap();
            let got = settings(&id).await;
            assert!(got
                .get("parameters")
                .is_none_or(|p| p.as_array().is_some_and(Vec::is_empty)));
            assert!(got.get("schedule").is_none());
            assert_eq!(
                got["trigger"]["periodic"],
                json!({ "interval": 2, "unit": "DAYS" })
            );
            assert_eq!(got["tags"], json!({ "dbine": "test" }));

            // A stale form is refused.
            assert!(s
                .scheduler_script("job", Some(&id), &back(&f))
                .await
                .is_err());

            // Run from a task: it and what comes after.
            s.run_job_from(&id, "dbine_test_sched_b").await.unwrap();
            let mut done = None;
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let runs = s.job_runs(&id, 5).await.unwrap();
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
            let ran: Vec<&str> = r
                .steps
                .iter()
                .filter(|s| s.outcome == JobOutcome::Succeeded)
                .map(|s| s.name.as_str())
                .collect();
            assert!(ran.contains(&"dbine_test_sched_b"), "{ran:?}");
            assert!(!ran.contains(&"dbine_test_sched_a2"), "{ran:?}");

            s.clear_job_history(&id).await.unwrap();
            assert!(s.job_runs(&id, 5).await.unwrap().is_empty());

            let d = s.scheduler_delete_script("job", &id).await.unwrap();
            assert!(d.contains("-- POST /api/2.1/jobs/delete"), "{d}");
            s.scheduler_delete("job", &id).await.unwrap();
            assert!(s.scheduler_form("job", Some(&id)).await.is_err());
        })
    };
    let outcome = body.await;
    // Cleanup before reporting.
    cleanup(&prefix).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

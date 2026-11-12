//! Server jobs (tasks) against a real Snowflake account (no emulator; same
//! variables as tests/processes.rs, a role that can create a database and
//! tasks and execute them, and a warehouse for the history):
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=… DBINE_TEST_SNOWFLAKE_USER=… DBINE_TEST_SNOWFLAKE_TOKEN=… \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test jobs -- --ignored --nocapture
//! ```
//!
//! It makes its own database with a two-task graph and drops it at the end,
//! also when an assertion fails.

use dbine_driver::{ConnectionConfig, JobOutcome, QueryOutcome, Session};
use std::time::Duration;

const DB: &str = "DBINE_TEST_JOBS_DB";
const ROOT: &str = "DBINE_TEST_JOBS_ROOT";
const CHILD: &str = "DBINE_TEST_JOBS_CHILD";

fn config() -> Option<ConnectionConfig> {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok();
    let mut options = std::collections::HashMap::from([
        ("account".to_string(), var("ACCOUNT")?),
        ("token".to_string(), var("TOKEN")?),
    ]);
    options.insert("warehouse".into(), var("WAREHOUSE")?);
    Some(ConnectionConfig {
        driver: "snowflake".into(),
        username: var("USER"),
        database: DB.into(),
        options: options.into_iter().collect(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map(|_| out)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tasks_are_listed_run_disabled_and_scripted() {
    let Some(cfg) = config() else {
        eprintln!("sin DBINE_TEST_SNOWFLAKE_*: se omite");
        return;
    };
    let d = dbine_driver_snowflake::drivers().remove(0);
    assert!(d.capabilities().jobs);
    let boot = ConnectionConfig {
        database: String::new(),
        ..cfg.clone()
    };
    let mut admin = d.connect(&boot, None).await.expect("connect");
    let wh = cfg.option("warehouse").unwrap().to_string();
    // The history needs a running warehouse and never resumes one itself.
    run(
        &mut admin,
        &format!("ALTER WAREHOUSE {wh} RESUME IF SUSPENDED"),
    )
    .await
    .unwrap();
    run(&mut admin, &format!("CREATE DATABASE IF NOT EXISTS {DB}"))
        .await
        .unwrap();

    let body = {
        let (d, cfg) = (d.clone(), cfg.clone());
        tokio::spawn(async move {
            let mut s = d.connect(&cfg, None).await.expect("connect");
            run(&mut s, &format!(
                "CREATE OR REPLACE TASK PUBLIC.{ROOT} WAREHOUSE = {wh} SCHEDULE = 'USING CRON 0 3 * * * UTC' COMMENT = 'dbine' AS SELECT 1;
                 CREATE OR REPLACE TASK PUBLIC.{CHILD} WAREHOUSE = {wh} AFTER PUBLIC.{ROOT} AS SELECT 2;
                 ALTER TASK PUBLIC.{CHILD} RESUME;"
            ))
            .await
            .unwrap();
            let id = format!("\"{DB}\".\"PUBLIC\".\"{ROOT}\"");

            // List: the root is the job, the child one of its steps.
            let list = s.jobs().await.unwrap();
            eprintln!("{:#?}", list.notes);
            let job = list
                .jobs
                .iter()
                .find(|j| j.id == id)
                .expect("the root is listed");
            assert!(
                !list.jobs.iter().any(|j| j.name == CHILD),
                "the child is a step"
            );
            assert_eq!(job.kind, "Task");
            assert_eq!(job.description.as_deref(), Some("dbine"));
            assert!(!job.enabled && job.can_run && job.can_enable && job.has_history);

            // Detail.
            let detail = s.job_detail(&id).await.unwrap();
            let steps: Vec<&str> = detail.steps.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(steps, [ROOT, CHILD]);

            // Run now, then the run is in the history with its steps.
            s.run_job(&id).await.unwrap();
            let mut done = None;
            let mut last_error = None;
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_secs(5)).await;
                // The warehouse may still be resuming: not yet.
                let runs = match s.job_runs(&id, 10).await {
                    Ok(r) => r,
                    Err(e) => {
                        last_error = Some(e.to_string());
                        continue;
                    }
                };
                if let Some(r) = runs.first().filter(|r| r.outcome != JobOutcome::Running) {
                    if r.steps.len() == 2 {
                        done = Some(r.clone());
                        break;
                    }
                }
            }
            let r = done.unwrap_or_else(|| {
                panic!("the run shows in the history (last error: {last_error:?})")
            });
            eprintln!("{r:#?}");
            assert_eq!(r.outcome, JobOutcome::Succeeded);
            assert_eq!(r.trigger.as_deref(), Some("manual"));
            assert!(r.started.as_deref().is_some_and(|t| t.ends_with('Z')));
            // The list and the detail fold the graph's run as the history does.
            let listed = s.jobs().await.unwrap();
            let listed = listed.jobs.iter().find(|j| j.id == id).unwrap();
            assert_eq!(listed.last_outcome, Some(r.outcome));
            assert_eq!(listed.last_duration_ms, r.duration_ms);
            let detail = s.job_detail(&id).await.unwrap();
            assert_eq!(detail.job.last_outcome, Some(r.outcome));

            // Enable and disable.
            s.set_job_enabled(&id, true).await.unwrap();
            let on = s.job_detail(&id).await.unwrap().job;
            assert!(on.enabled);
            s.set_job_enabled(&id, false).await.unwrap();
            assert!(!s.job_detail(&id).await.unwrap().job.enabled);

            // Script: the graph, resuming the child.
            let script = s.job_script(&id).await.unwrap();
            eprintln!("{script}");
            assert!(script.to_lowercase().contains("create or replace task"));
            assert!(script.contains(CHILD));
            assert!(script.contains(&format!(
                "ALTER TASK \"{DB}\".\"PUBLIC\".\"{CHILD}\" RESUME;"
            )));

            // Stop is accepted with nothing running.
            s.stop_job(&id).await.unwrap();

            // A crafted id is refused before anything runs.
            let evil = format!("x'); DROP DATABASE {DB}; --");
            assert!(s.run_job(&evil).await.is_err());
            assert!(s.set_job_enabled(&evil, true).await.is_err());
            assert!(s.job_runs(&evil, 1).await.is_err());
            assert!(s
                .job_detail(&format!(
                    "\"{DB}\".\"PUBLIC\".\"{ROOT}\"; DROP DATABASE {DB}"
                ))
                .await
                .is_err());
            assert!(s.list_databases().await.unwrap().iter().any(|d| d == DB));
        })
    };
    let outcome = body.await;
    // Cleanup before reporting: the database goes with its tasks.
    run(&mut admin, &format!("DROP DATABASE IF EXISTS {DB}"))
        .await
        .unwrap();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

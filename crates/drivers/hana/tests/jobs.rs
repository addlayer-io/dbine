//! Scheduler jobs against a real SAP HANA Cloud (no Docker image: see
//! tests/integration.rs). The user needs CREATE ANY on its own schema and
//! SESSION ADMIN to stop a run. Skipped without the URL:
//!
//! ```sh
//! DBINE_TEST_HANA_URL=hana://USER:PASSWORD@host:443?tls \
//!   cargo test -p dbine-driver-hana --test jobs -- --ignored
//! ```
//!
//! The test makes its own `DBINE_TEST_JOBS_*` procedure and job in the
//! user's schema and drops them at the end, also when an assertion fails.

use dbine_driver::{ConnectionConfig, JobOutcome, QueryOutcome, Session};
use std::time::Duration;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_HANA_URL").ok()?;
    let rest = url.strip_prefix("hana://")?;
    let (rest, tls) = match rest.split_once('?') {
        Some((r, q)) => (r, q.contains("tls")),
        None => (rest, false),
    };
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (host, port) = addr.split_once(':')?;
    Some(ConnectionConfig {
        driver: "hana".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        encrypt: tls,
        ..Default::default()
    })
}

async fn open(cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_hana::drivers()
        .into_iter()
        .find(|d| d.info().id == "hana")
        .unwrap();
    assert!(d.capabilities().jobs);
    d.connect(cfg, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn clean(cfg: &ConnectionConfig) {
    let mut s = open(cfg).await;
    let _ = run(&mut s, "DROP SCHEDULER JOB DBINE_TEST_JOBS_EVERY_MINUTE").await;
    let _ = run(&mut s, "DROP PROCEDURE DBINE_TEST_JOBS_PROC").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn hana_scheduler_jobs() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_HANA_URL not set; skipping");
        return;
    };
    clean(&cfg).await;
    let c = cfg.clone();
    let body = tokio::spawn(async move { body(&c).await }).await;
    clean(&cfg).await;
    body.unwrap();
}

async fn body(cfg: &ConnectionConfig) {
    let mut s = open(cfg).await;
    let list = s.jobs().await.unwrap();
    if list.jobs.is_empty() && list.notes.iter().any(|n| n.contains("no tiene")) {
        eprintln!("this server has no scheduler jobs: {list:?}");
        return;
    }
    run(
        &mut s,
        "CREATE PROCEDURE DBINE_TEST_JOBS_PROC(IN P_N INTEGER) AS BEGIN SELECT :P_N FROM DUMMY; END",
    )
    .await
    .unwrap();
    // Every minute, at second 0.
    run(
        &mut s,
        "CREATE SCHEDULER JOB DBINE_TEST_JOBS_EVERY_MINUTE CRON '* * * * * * 0' ENABLE PROCEDURE DBINE_TEST_JOBS_PROC PARAMETERS P_N = 3",
    )
    .await
    .unwrap();
    let j = s
        .jobs()
        .await
        .unwrap()
        .jobs
        .into_iter()
        .find(|j| j.name == "DBINE_TEST_JOBS_EVERY_MINUTE")
        .expect("the job is listed");
    eprintln!("{j:#?}");
    assert_eq!(j.schedule.as_deref(), Some("* * * * * * 0"));
    assert!(j.enabled && j.can_enable && j.has_script && !j.can_run);

    let d = s.job_detail(&j.id).await.unwrap();
    assert!(d.steps[0]
        .command
        .as_deref()
        .unwrap_or("")
        .contains("DBINE_TEST_JOBS_PROC"));

    // A minute later it has run.
    if j.has_history {
        let mut runs = Vec::new();
        for _ in 0..30 {
            runs = s.job_runs(&j.id, 10).await.unwrap();
            if runs.iter().any(|r| r.outcome == JobOutcome::Succeeded) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        eprintln!("{runs:#?}");
        assert!(runs.iter().any(|r| r.outcome == JobOutcome::Succeeded));
    }

    s.set_job_enabled(&j.id, false).await.unwrap();
    assert!(!s.job_detail(&j.id).await.unwrap().job.enabled);
    s.set_job_enabled(&j.id, true).await.unwrap();

    let script = s.job_script(&j.id).await.unwrap();
    eprintln!("{script}");
    assert!(script.starts_with("CREATE SCHEDULER JOB"));
    run(&mut s, "DROP SCHEDULER JOB DBINE_TEST_JOBS_EVERY_MINUTE")
        .await
        .unwrap();
    run(&mut s, script.trim_end().trim_end_matches(';'))
        .await
        .unwrap();
    assert!(s.job_detail(&j.id).await.is_ok());

    for bad in ["x' OR 1=1 --", "\"S\".\"J\"; DROP TABLE X", "S.J"] {
        assert!(s.job_detail(bad).await.is_err(), "{bad}");
        assert!(s.set_job_enabled(bad, false).await.is_err(), "{bad}");
    }
    let e = s.job_detail("\"SYS\".\"x' OR '1'='1\"").await.unwrap_err();
    assert!(e.to_string().contains("no existe"), "{e}");
}

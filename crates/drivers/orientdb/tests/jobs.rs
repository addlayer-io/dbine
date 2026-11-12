//! Server jobs (the scheduler's events) against a real server (see
//! tests/integration.rs, which creates the `dbine_admin` database):
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test jobs -- --ignored
//! ```
//!
//! The test makes its own class, function and event (`dbine_test_jobs_*`)
//! and deletes them at the end, also when an assertion fails.

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::{Duration, Instant};

const EVENT: &str = "dbine_test_jobs_event";
const FUNC: &str = "dbine_test_jobs_fn";
const CLASS: &str = "DbineTestJobsLog";

fn cfg(url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    }
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_orientdb::drivers().remove(0);
    match d.connect(c, Some("dbine_admin")).await {
        Ok(s) => s,
        Err(_) => {
            let _ = d
                .connect(c, None)
                .await
                .expect("connect")
                .create_database("dbine_admin")
                .await;
            d.connect(c, Some("dbine_admin")).await.expect("connect")
        }
    }
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await?;
    match out.error.take() {
        Some(e) => Err(dbine_driver::Error::Query(e)),
        None => Ok(out),
    }
}

async fn cleanup(c: &ConnectionConfig) {
    let mut s = open(c).await;
    let _ = exec(
        &mut s,
        &format!("DELETE FROM OSchedule WHERE name = '{EVENT}'"),
    )
    .await;
    let _ = exec(
        &mut s,
        &format!("DELETE FROM OFunction WHERE name = '{FUNC}'"),
    )
    .await;
    let _ = exec(&mut s, &format!("DROP CLASS {CLASS} IF EXISTS UNSAFE")).await;
}

async fn runs_of(s: &mut Box<dyn Session>) -> u64 {
    s.job_detail(EVENT)
        .await
        .expect("detail")
        .properties
        .iter()
        .find(|p| p.label == "Ejecuciones")
        .and_then(|p| p.value.parse().ok())
        .unwrap_or(0)
}

async fn body(c: ConnectionConfig) {
    let d = dbine_driver_orientdb::drivers().remove(0);
    assert!(d.capabilities().jobs);
    let mut s = open(&c).await;
    exec(&mut s, &format!("CREATE CLASS {CLASS} IF NOT EXISTS"))
        .await
        .expect("class");
    exec(
        &mut s,
        &format!("CREATE FUNCTION {FUNC} \"INSERT INTO {CLASS} SET at = sysdate()\" LANGUAGE SQL"),
    )
    .await
    .expect("function");
    exec(
        &mut s,
        &format!("INSERT INTO OSchedule SET name = '{EVENT}', rule = '0/2 * * * * ?', function = (SELECT FROM OFunction WHERE name = '{FUNC}'), starttime = sysdate()"),
    )
    .await
    .expect("event");

    // Listed, with what each action allows (only reading).
    let list = s.jobs().await.expect("jobs");
    let job = list
        .jobs
        .iter()
        .find(|j| j.id == EVENT)
        .expect("the event is listed")
        .clone();
    assert_eq!(job.kind, "Scheduled event");
    assert_eq!(job.schedule.as_deref(), Some("0/2 * * * * ?"));
    assert_eq!(job.group.as_deref(), Some("dbine_admin"));
    assert!(job.enabled && job.has_script);
    assert!(!job.can_run && !job.can_stop && !job.can_enable && !job.has_history);

    // It runs on its own: the run count grows and the last start shows.
    let start = Instant::now();
    while runs_of(&mut s).await < 2 {
        assert!(start.elapsed() < Duration::from_secs(30), "it never ran");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let det = s.job_detail(EVENT).await.expect("detail");
    assert!(det
        .job
        .last_started
        .as_deref()
        .is_some_and(|t| t.ends_with('Z')));
    assert_eq!(det.steps[0].name, FUNC);
    assert!(det.steps[0]
        .command
        .as_deref()
        .is_some_and(|c| c.contains(CLASS)));

    // Script: deleted and run, the event comes back.
    let script = s.job_script(EVENT).await.expect("script");
    assert!(script.contains("CREATE FUNCTION") && script.contains("INSERT INTO OSchedule"));
    exec(
        &mut s,
        &format!("DELETE FROM OSchedule WHERE name = '{EVENT}'"),
    )
    .await
    .expect("delete");
    assert!(!s.jobs().await.unwrap().jobs.iter().any(|j| j.id == EVENT));
    exec(&mut s, &script)
        .await
        .expect("the script recreates it");
    let back = s.jobs().await.unwrap();
    let again = back.jobs.iter().find(|j| j.id == EVENT).expect("back");
    assert_eq!(again.schedule.as_deref(), Some("0/2 * * * * ?"));

    // Nothing that changes an event is there.
    assert!(s.run_job(EVENT).await.is_err());
    assert!(s.stop_job(EVENT).await.is_err());
    assert!(s.set_job_enabled(EVENT, false).await.is_err());
    assert!(s.job_runs(EVENT, 10).await.is_err());

    // A malicious or unknown id only finds nothing.
    for bad in [
        "dbine_test_jobs_event' OR '1'='1",
        "x'; DELETE FROM OSchedule; --",
        "dbine_test_jobs_nope",
        "",
    ] {
        assert!(s.job_detail(bad).await.is_err(), "{bad:?}");
        assert!(s.job_script(bad).await.is_err(), "{bad:?}");
    }
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == EVENT));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn scheduled_events_as_jobs() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else {
        eprintln!("DBINE_TEST_ORIENTDB_URL not set; skipping");
        return;
    };
    let c = cfg(&url);
    cleanup(&c).await;
    let r = tokio::spawn(body(c.clone())).await;
    cleanup(&c).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

//! Server jobs (reflections and their refreshes, run now and stopped)
//! against Dremio OSS.
//! Skipped without `DBINE_TEST_DREMIO_URL` (the user is the one
//! tests/processes.rs bootstraps):
//!
//! ```sh
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 \
//!   cargo test -p dbine-driver-dremio --test jobs -- --ignored
//! ```
//!
//! The test makes its own table and reflection in `$scratch`
//! (`dbine_test_jobs_*`) and drops them at the end, also when an assertion
//! fails.

use dbine_driver::{ConnectionConfig, JobOutcome, QueryOutcome, Session};
use std::time::{Duration, Instant};

const TABLE: &str = "\"$scratch\".dbine_test_jobs_t";
const REFLECTION: &str = "dbine_test_jobs_r";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        password: Some("secreto123".into()),
        ..Default::default()
    })
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_dremio::drivers()[0]
        .connect(c, None)
        .await
        .expect("connect")
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<()> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await?;
    match out.error {
        Some(e) => Err(dbine_driver::Error::Query(e)),
        None => Ok(()),
    }
}

async fn cleanup(c: &ConnectionConfig) {
    let mut s = open(c).await;
    let _ = exec(
        &mut s,
        &format!("ALTER TABLE {TABLE} DROP REFLECTION {REFLECTION}"),
    )
    .await;
    let _ = exec(&mut s, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
}

async fn the_job(s: &mut Box<dyn Session>) -> Option<dbine_driver::ServerJob> {
    s.jobs()
        .await
        .expect("jobs")
        .jobs
        .into_iter()
        .find(|j| j.name == REFLECTION)
}

async fn body(c: ConnectionConfig) {
    let d = &dbine_driver_dremio::drivers()[0];
    assert!(d.capabilities().jobs);
    let mut s = open(&c).await;
    // Ten million rows, so the first (full) refresh lasts seconds: long
    // enough to stop it. Later refreshes of an unchanged table are
    // incremental no-ops of a few hundred ms.
    let digits = "(VALUES (0), (1), (2), (3), (4), (5), (6), (7), (8), (9))";
    exec(
        &mut s,
        &format!(
            "CREATE TABLE {TABLE} AS SELECT d1.x * 1000000 + d2.x * 100000 + d3.x * 10000 + d4.x * 1000 + d5.x * 100 + d6.x * 10 + d7.x AS a, CAST(d1.x AS VARCHAR) || 'dbine' AS b FROM {digits} d1(x), {digits} d2(x), {digits} d3(x), {digits} d4(x), {digits} d5(x), {digits} d6(x), {digits} d7(x)"
        ),
    )
    .await
    .expect("table");
    exec(
        &mut s,
        &format!("ALTER TABLE {TABLE} CREATE RAW REFLECTION {REFLECTION} USING DISPLAY (a, b)"),
    )
    .await
    .expect("reflection");

    // Its first refresh shows as running, Running being its last outcome
    // (none finished before), and can be stopped.
    let start = Instant::now();
    let job = loop {
        if let Some(j) = the_job(&mut s).await {
            if j.can_stop {
                break j;
            }
            assert!(
                j.last_outcome.is_none() || j.last_outcome == Some(JobOutcome::Running),
                "the first refresh ended before it could be stopped: {j:?}"
            );
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the first refresh never showed as running"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(job.running, "{job:?}");
    assert_eq!(job.last_outcome, Some(JobOutcome::Running));
    s.stop_job(&job.id).await.expect("stop");
    let start = Instant::now();
    loop {
        let runs = s.job_runs(&job.id, 5).await.expect("runs");
        if runs.first().map(|r| r.outcome) == Some(JobOutcome::Canceled) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the refresh wasn't cancelled: {runs:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Run now: a new materialization shows in the history.
    let before = s.job_runs(&job.id, 50).await.expect("runs");
    assert!(!before.is_empty());
    s.run_job(&job.id).await.expect("run now");
    let start = Instant::now();
    loop {
        let runs = s.job_runs(&job.id, 50).await.expect("runs");
        let new = runs
            .iter()
            .filter(|r| !before.iter().any(|b| b.id == r.id))
            .find(|r| r.outcome == JobOutcome::Succeeded);
        if new.is_some() {
            assert!(runs[0].started >= before[0].started, "newest first");
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "no new refresh: {runs:?}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Listed with the source's policy, its last refresh done; nothing to
    // stop now.
    let job = the_job(&mut s).await.expect("listed");
    assert_eq!(job.kind, "Reflection");
    assert_eq!(job.group.as_deref(), Some("\"$scratch\".dbine_test_jobs_t"));
    assert!(
        job.schedule
            .as_deref()
            .is_some_and(|t| t.starts_with("EVERY ")),
        "{job:?}"
    );
    assert!(job.enabled && job.can_run && job.has_history && job.has_script);
    assert!(!job.running && !job.can_stop && !job.can_enable && !job.system);
    assert_eq!(job.last_outcome, Some(JobOutcome::Succeeded));
    assert!(job.last_started.is_some());
    let e = s.stop_job(&job.id).await.expect_err("nothing running");
    assert!(e.to_string().contains("no se está refrescando"), "{e}");

    // Detail.
    let det = s.job_detail(&job.id).await.expect("detail");
    assert!(det.properties.iter().any(|p| p.label == "Refresco"));
    assert!(det
        .notes
        .iter()
        .any(|n| n.contains("todas las reflexiones")));

    // Script: dropped and run, the reflection comes back.
    let script = s.job_script(&job.id).await.expect("script");
    assert!(
        script.contains("CREATE RAW REFLECTION \"dbine_test_jobs_r\""),
        "{script}"
    );
    exec(
        &mut s,
        &format!("ALTER TABLE {TABLE} DROP REFLECTION {REFLECTION}"),
    )
    .await
    .expect("drop");
    exec(&mut s, script.trim_end().trim_end_matches(';'))
        .await
        .expect("the script recreates it");
    assert!(the_job(&mut s).await.is_some());

    // No enabling.
    assert!(s.set_job_enabled(&job.id, false).await.is_err());

    // A malicious or unknown id is refused before anything runs.
    for bad in [
        "reflection:x' OR '1'='1",
        "reflection:00000000-0000-0000-0000-000000000000",
        "dbine_test_jobs_r",
        "",
    ] {
        assert!(s.job_detail(bad).await.is_err(), "{bad:?}");
        assert!(s.run_job(bad).await.is_err(), "{bad:?}");
        assert!(s.stop_job(bad).await.is_err(), "{bad:?}");
        assert!(s.job_runs(bad, 5).await.is_err(), "{bad:?}");
        assert!(s.job_script(bad).await.is_err(), "{bad:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn reflections_as_jobs() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_DREMIO_URL not set; skipping");
        return;
    };
    cleanup(&c).await;
    let r = tokio::spawn(body(c.clone())).await;
    cleanup(&c).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

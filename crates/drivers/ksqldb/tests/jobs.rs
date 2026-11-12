//! Server jobs (persistent queries) against a real ksqlDB
//! (`DBINE_TEST_KSQLDB_URL`, see tests/integration.rs for the containers);
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_KSQLDB_URL=http://localhost:25188 \
//!   cargo test -p dbine-driver-ksqldb --test jobs -- --ignored
//! ```
//!
//! The test makes its own streams (`dbine_test_jobs_*`) and drops them at
//! the end, also when an assertion fails.

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::{Duration, Instant};

const SRC: &str = "DBINE_TEST_JOBS_SRC";
const SINK: &str = "DBINE_TEST_JOBS_SINK";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_KSQLDB_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "ksqldb".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        ..Default::default()
    })
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_ksqldb::drivers()[0]
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

/// Drops what the test made: the query that feeds the sink, then both
/// streams with their topics.
async fn cleanup(c: &ConnectionConfig) {
    let mut s = open(c).await;
    if let Ok(list) = s.jobs().await {
        for j in list
            .jobs
            .iter()
            .filter(|j| j.group.as_deref() == Some(SINK))
        {
            let _ = s.stop_job(&j.id).await;
        }
    }
    let _ = exec(
        &mut s,
        &format!("DROP STREAM IF EXISTS {SINK} DELETE TOPIC"),
    )
    .await;
    let _ = exec(&mut s, &format!("DROP STREAM IF EXISTS {SRC} DELETE TOPIC")).await;
}

async fn the_job(s: &mut Box<dyn Session>) -> Option<dbine_driver::ServerJob> {
    s.jobs()
        .await
        .expect("jobs")
        .jobs
        .into_iter()
        .find(|j| j.group.as_deref() == Some(SINK))
}

/// Waits until the job's `enabled` is `want` (PAUSE / RESUME take a moment).
async fn wait_enabled(s: &mut Box<dyn Session>, want: bool) {
    let start = Instant::now();
    loop {
        let j = the_job(s).await.expect("the job is listed");
        if j.enabled == want {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the job never became enabled={want}: {j:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn body(c: ConnectionConfig) {
    let d = &dbine_driver_ksqldb::drivers()[0];
    assert!(d.capabilities().jobs);
    let mut s = open(&c).await;
    exec(
        &mut s,
        &format!("CREATE STREAM {SRC} (id INT KEY, v VARCHAR) WITH (KAFKA_TOPIC='dbine_test_jobs_src', VALUE_FORMAT='JSON', PARTITIONS=1)"),
    )
    .await
    .expect("source stream");
    exec(
        &mut s,
        &format!("CREATE STREAM {SINK} AS SELECT * FROM {SRC} EMIT CHANGES"),
    )
    .await
    .expect("persistent query");

    // Listed, enabled, running, with what each action allows.
    let job = the_job(&mut s)
        .await
        .expect("the persistent query is listed");
    assert_eq!(job.kind, "Persistent query");
    assert!(job.enabled && job.can_stop && job.can_enable && job.has_script);
    assert!(!job.can_run && !job.has_history && !job.system);

    // Detail: the statement as the step, and the TERMINATE note.
    let det = s.job_detail(&job.id).await.expect("detail");
    assert!(det.steps[0]
        .command
        .as_deref()
        .is_some_and(|c| c.contains(SINK) && c.contains(SRC)));
    assert!(det.notes.iter().any(|n| n.contains("TERMINATE")));
    assert!(det
        .properties
        .iter()
        .any(|p| p.label == "Lee de" && p.value == SRC));

    // Script: the statement that made it.
    let script = s.job_script(&job.id).await.expect("script");
    assert!(script.starts_with("CREATE STREAM") && script.trim_end().ends_with(';'));

    // No run now, no history.
    assert!(s.run_job(&job.id).await.is_err());
    assert!(s.job_runs(&job.id, 10).await.is_err());

    // Disable (PAUSE) and enable (RESUME).
    s.set_job_enabled(&job.id, false).await.expect("pause");
    wait_enabled(&mut s, false).await;
    s.set_job_enabled(&job.id, true).await.expect("resume");
    wait_enabled(&mut s, true).await;

    // A malicious or unknown id is refused before anything runs.
    for bad in [
        "CSAS_X; DROP STREAM DBINE_TEST_JOBS_SRC",
        "CSAS X",
        "",
        "CSAS_DBINE_TEST_JOBS_NOPE_1",
    ] {
        assert!(s.job_detail(bad).await.is_err(), "{bad:?}");
        assert!(s.set_job_enabled(bad, false).await.is_err(), "{bad:?}");
        assert!(s.stop_job(bad).await.is_err(), "{bad:?}");
        assert!(s.job_script(bad).await.is_err(), "{bad:?}");
    }
    assert!(
        the_job(&mut s).await.is_some(),
        "the bad ids touched nothing"
    );

    // Stop = TERMINATE: the query is gone.
    s.stop_job(&job.id).await.expect("terminate");
    let start = Instant::now();
    while the_job(&mut s).await.is_some() {
        assert!(start.elapsed() < Duration::from_secs(30), "still listed");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn persistent_queries_as_jobs() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_KSQLDB_URL not set; skipping");
        return;
    };
    cleanup(&c).await;
    let r = tokio::spawn(body(c.clone())).await;
    cleanup(&c).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

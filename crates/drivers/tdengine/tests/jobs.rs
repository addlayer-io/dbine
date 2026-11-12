//! Server jobs (streams) against real TDengine servers, through
//! taosAdapter's REST port: the old stream engine (up to 3.3.6,
//! `DBINE_TEST_TDENGINE_URL`) and the new one (3.3.7 on,
//! `DBINE_TEST_TDENGINE_NEW_URL`, image `tdengine/tsdb`). Each is skipped
//! without its variable:
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//! DBINE_TEST_TDENGINE_NEW_URL=http://localhost:25741 \
//!   cargo test -p dbine-driver-tdengine --test jobs -- --ignored
//! ```
//!
//! Each test makes its own database and stream (`dbine_test_jobs_*`) and
//! drops them at the end, also when an assertion fails.

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::{Duration, Instant};

const DB: &str = "dbine_test_jobs";

fn cfg(env: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    })
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_tdengine::drivers()[0]
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

async fn cleanup(c: &ConnectionConfig, stream: &str) {
    let mut s = open(c).await;
    let _ = exec(&mut s, &format!("DROP STREAM IF EXISTS {stream}")).await;
    let _ = exec(&mut s, &format!("DROP DATABASE IF EXISTS {DB}")).await;
}

async fn the_job(s: &mut Box<dyn Session>, id: &str) -> Option<dbine_driver::ServerJob> {
    s.jobs()
        .await
        .expect("jobs")
        .jobs
        .into_iter()
        .find(|j| j.id == id)
}

/// Sets `enabled` and waits for the stream to report it: the change is
/// asynchronous, and a stream still starting up ignores it, so it's asked
/// again every few seconds.
async fn toggle(s: &mut Box<dyn Session>, id: &str, want: bool) {
    let start = Instant::now();
    let mut asked = Instant::now() - Duration::from_secs(60);
    loop {
        if asked.elapsed() > Duration::from_secs(5) {
            match s.set_job_enabled(id, want).await {
                Ok(()) => {}
                // Refused while the stream's tasks start: asked again later.
                Err(e) if e.to_string().contains("poniendo en marcha") => {}
                Err(e) => panic!("set_job_enabled: {e:?}"),
            }
            asked = Instant::now();
        }
        let j = the_job(s, id).await.expect("the stream is listed");
        if j.enabled == want {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the stream never became enabled={want}: {j:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn body(c: ConnectionConfig, new_engine: bool) {
    let d = &dbine_driver_tdengine::drivers()[0];
    assert!(d.capabilities().jobs);
    let mut s = open(&c).await;
    exec(&mut s, &format!("CREATE DATABASE IF NOT EXISTS {DB}"))
        .await
        .expect("database");
    exec(
        &mut s,
        &format!(
            "CREATE STABLE IF NOT EXISTS {DB}.meters (ts TIMESTAMP, v INT) TAGS (loc VARCHAR(16))"
        ),
    )
    .await
    .expect("stable");
    let (create, id) = if new_engine {
        (
            format!("CREATE STREAM {DB}.dbine_test_jobs_s INTERVAL(10s) SLIDING(10s) FROM {DB}.meters INTO {DB}.res_t AS SELECT _twstart AS ts, count(*) AS c FROM {DB}.meters WHERE ts >= _twstart AND ts < _twend"),
            format!("{DB}.dbine_test_jobs_s"),
        )
    } else {
        (
            format!("CREATE STREAM IF NOT EXISTS dbine_test_jobs_s INTO {DB}.res_t AS SELECT _wstart, count(*) AS c FROM {DB}.meters INTERVAL(10s)"),
            "dbine_test_jobs_s".to_string(),
        )
    };
    exec(&mut s, &create).await.expect("stream");

    // Listed, with what each action allows.
    let job = the_job(&mut s, &id).await.expect("the stream is listed");
    assert_eq!(job.kind, "Stream");
    assert_eq!(job.group.as_deref(), Some(DB));
    assert!(job.can_enable && job.has_script && job.enabled);
    assert!(!job.can_run && !job.can_stop && !job.has_history && !job.system);
    if new_engine {
        assert_eq!(job.schedule.as_deref(), Some("interval(10s) sliding(10s)"));
    } else {
        assert!(job
            .schedule
            .as_deref()
            .is_some_and(|t| t.starts_with("TRIGGER ")));
    }

    // Detail: the statement as the step, the state among the properties.
    let det = s.job_detail(&id).await.expect("detail");
    assert!(det.steps[0]
        .command
        .as_deref()
        .is_some_and(|c| c.to_lowercase().contains("create stream")));
    assert!(det.properties.iter().any(|p| p.label == "Estado"));
    assert!(!det.notes.is_empty());

    // Script: the statement that made it.
    let script = s.job_script(&id).await.expect("script");
    assert!(script.to_lowercase().starts_with("create stream") && script.trim_end().ends_with(';'));

    // No run now, no stop, no history.
    assert!(s.run_job(&id).await.is_err());
    assert!(s.stop_job(&id).await.is_err());
    assert!(s.job_runs(&id, 10).await.is_err());

    // Disable and enable.
    toggle(&mut s, &id, false).await;
    toggle(&mut s, &id, true).await;

    // A malicious or unknown id is refused before anything runs.
    for bad in [
        "dbine_test_jobs_s`; DROP DATABASE dbine_test_jobs; --",
        "dbine_test_jobs.nope",
        "",
        "x\ny",
    ] {
        assert!(s.job_detail(bad).await.is_err(), "{bad:?}");
        assert!(s.set_job_enabled(bad, false).await.is_err(), "{bad:?}");
        assert!(s.job_script(bad).await.is_err(), "{bad:?}");
    }
    assert!(
        the_job(&mut s, &id).await.is_some(),
        "the bad ids touched nothing"
    );
}

async fn run(env: &str, new_engine: bool) {
    let Some(c) = cfg(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let stream = if new_engine {
        format!("{DB}.dbine_test_jobs_s")
    } else {
        "dbine_test_jobs_s".to_string()
    };
    cleanup(&c, &stream).await;
    let r = tokio::spawn(body(c.clone(), new_engine)).await;
    cleanup(&c, &stream).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn streams_of_the_old_engine() {
    run("DBINE_TEST_TDENGINE_URL", false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn streams_of_the_new_engine() {
    run("DBINE_TEST_TDENGINE_NEW_URL", true).await;
}

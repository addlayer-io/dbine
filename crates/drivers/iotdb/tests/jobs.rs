//! Server jobs (continuous queries and pipes) against real servers (see
//! tests/integration.rs for the containers; user root / root): IoTDB 1.x
//! (`DBINE_TEST_IOTDB_URL`) and 2.x (`DBINE_TEST_IOTDB2_URL`). Each is
//! skipped without its variable:
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 DBINE_TEST_IOTDB2_URL=http://localhost:27150 \
//!   cargo test -p dbine-driver-iotdb --test jobs -- --ignored
//! ```
//!
//! Each test makes its own database, continuous query and pipe
//! (`dbine_test_jobs_*`) and drops them at the end, also when an assertion
//! fails.

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

const DB: &str = "root.dbine_test_jobs";
const CQ: &str = "dbine_test_jobs_cq";
const CQ_ID: &str = "cq:dbine_test_jobs_cq";
const PIPE: &str = "dbine_test_jobs_pipe";
const PIPE_ID: &str = "pipe:dbine_test_jobs_pipe";
const SECRET: &str = "dbine_test_jobs_secret";

fn cfg(env: &str) -> Option<ConnectionConfig> {
    Some(ConnectionConfig {
        driver: "iotdb".into(),
        host: std::env::var(env).ok()?,
        username: Some("root".into()),
        password: Some("root".into()),
        ..Default::default()
    })
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_iotdb::drivers()[0]
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
    let _ = exec(&mut s, &format!("DROP CQ {CQ}")).await;
    let _ = exec(&mut s, &format!("DROP PIPE {PIPE}")).await;
    let _ = exec(&mut s, &format!("DROP DATABASE {DB}")).await;
}

async fn body(c: ConnectionConfig) {
    let d = &dbine_driver_iotdb::drivers()[0];
    assert!(d.capabilities().jobs);
    let mut s = open(&c).await;
    exec(&mut s, &format!("CREATE DATABASE {DB}"))
        .await
        .expect("database");
    exec(
        &mut s,
        &format!("CREATE TIMESERIES {DB}.d1.s1 WITH DATATYPE=INT64, ENCODING=PLAIN"),
    )
    .await
    .expect("series");
    let create = format!("CREATE CONTINUOUS QUERY {CQ} RESAMPLE EVERY 30s RANGE 1m BEGIN SELECT count(s1) INTO {DB}.agg(c) FROM {DB}.d1 GROUP BY(30s) END");
    exec(&mut s, &create).await.expect("cq");

    // Listed, with what each action allows (only reading).
    let list = s.jobs().await.expect("jobs");
    let job = list
        .jobs
        .iter()
        .find(|j| j.id == CQ_ID)
        .expect("the CQ is listed")
        .clone();
    assert_eq!(job.kind, "Continuous query");
    assert_eq!(job.schedule.as_deref(), Some("EVERY 30s"));
    assert!(job.enabled && job.has_script);
    assert!(!job.can_run && !job.can_stop && !job.can_enable && !job.has_history);

    // Detail: the SELECT … INTO as the step, the window as a property.
    let det = s.job_detail(CQ_ID).await.expect("detail");
    assert!(det.steps[0]
        .command
        .as_deref()
        .is_some_and(|q| q.starts_with("SELECT count(s1) INTO")));
    assert!(det
        .properties
        .iter()
        .any(|p| p.label == "Ventana (RANGE)" && p.value == "1m"));

    // Script: the statement it was made with; dropped and run, it comes back.
    let script = s.job_script(CQ_ID).await.expect("script");
    assert!(script.starts_with("CREATE CONTINUOUS QUERY") && script.trim_end().ends_with(';'));
    exec(&mut s, &format!("DROP CQ {CQ}")).await.expect("drop");
    exec(&mut s, script.trim_end().trim_end_matches(';'))
        .await
        .expect("the script recreates it");
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == CQ_ID));

    // Nothing that changes a CQ is there.
    assert!(s.run_job(CQ_ID).await.is_err());
    assert!(s.stop_job(CQ_ID).await.is_err());
    assert!(s.set_job_enabled(CQ_ID, false).await.is_err());
    assert!(s.job_runs(CQ_ID, 10).await.is_err());

    // A pipe: listed running, stopped and started, its script recreates it.
    // Its password (IoTDB 1.x shows it as typed, 2.x as `******`) shows
    // nowhere.
    exec(
        &mut s,
        &format!("CREATE PIPE {PIPE} WITH SOURCE (\"source.pattern\"=\"{DB}.**\") WITH SINK (\"sink\"=\"do-nothing-sink\", \"sink.password\"=\"{SECRET}\")"),
    )
    .await
    .expect("pipe");
    let pipe = |l: &dbine_driver::JobList| l.jobs.iter().find(|j| j.id == PIPE_ID).cloned();
    let p = pipe(&s.jobs().await.unwrap()).expect("the pipe is listed");
    assert_eq!(p.kind, "Pipe");
    assert!(p.enabled && p.can_enable && p.has_script && !p.system);
    assert!(!p.can_run && !p.can_stop && !p.has_history);
    s.set_job_enabled(PIPE_ID, false).await.expect("stop pipe");
    assert!(!pipe(&s.jobs().await.unwrap()).unwrap().enabled);
    s.set_job_enabled(PIPE_ID, true).await.expect("start pipe");
    assert!(pipe(&s.jobs().await.unwrap()).unwrap().enabled);
    let det = s.job_detail(PIPE_ID).await.expect("pipe detail");
    assert!(det
        .properties
        .iter()
        .any(|p| p.group == "Destino" && p.value == "do-nothing-sink"));
    assert!(det
        .properties
        .iter()
        .any(|p| p.label == "sink.password" && p.value == "******"));
    assert!(det.notes.iter().any(|n| n.contains("******")));
    let script = s.job_script(PIPE_ID).await.expect("pipe script");
    assert!(
        script.starts_with("CREATE PIPE `dbine_test_jobs_pipe`"),
        "{script}"
    );
    assert!(
        !format!("{det:?}{script}").contains(SECRET),
        "the password shows: {det:?} {script}"
    );
    exec(&mut s, &format!("DROP PIPE {PIPE}"))
        .await
        .expect("drop pipe");
    exec(&mut s, script.trim_end().trim_end_matches(';'))
        .await
        .expect("the pipe script recreates it");
    assert!(pipe(&s.jobs().await.unwrap()).is_some());
    assert!(s.run_job(PIPE_ID).await.is_err());
    assert!(s.job_runs(PIPE_ID, 10).await.is_err());

    // A malicious or unknown id only finds nothing.
    for bad in [
        "cq:dbine_test_jobs_cq; DROP DATABASE root.dbine_test_jobs",
        "pipe:dbine_test_jobs_pipe; DROP DATABASE root.dbine_test_jobs",
        "pipe:dbine_test_jobs_pipe`; DROP PIPE dbine_test_jobs_pipe; --",
        "dbine_test_jobs_cq",
        "cq:dbine_test_jobs_nope",
        "",
    ] {
        assert!(s.job_detail(bad).await.is_err(), "{bad:?}");
        assert!(s.job_script(bad).await.is_err(), "{bad:?}");
        assert!(s.set_job_enabled(bad, false).await.is_err(), "{bad:?}");
    }
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == PIPE_ID));
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == CQ_ID));
}

async fn run(env: &str) {
    let Some(c) = cfg(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    cleanup(&c).await;
    let r = tokio::spawn(body(c.clone())).await;
    cleanup(&c).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn jobs_on_iotdb_1() {
    run("DBINE_TEST_IOTDB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn jobs_on_iotdb_2() {
    run("DBINE_TEST_IOTDB2_URL").await;
}

//! Server jobs against real servers. Each test reads
//! `DBINE_TEST_<ENGINE>_URL` (`user:pass@host:port` or `host:port`) and is
//! skipped without it:
//!
//! - `DBINE_TEST_NEO4J_APOC_URL`: Neo4j 5 with APOC, e.g.
//!   `docker run -d --name dbine-test-jobs-neo4j -p 17688:7687
//!   -e NEO4J_AUTH=neo4j/dbine-test-pass -e NEO4J_PLUGINS='["apoc"]' neo4j:5`
//!   and `neo4j:dbine-test-pass@localhost:17688`;
//! - `DBINE_TEST_NEO4J_URL`: Neo4j without APOC (an empty list and a note);
//! - `DBINE_TEST_MEMGRAPH_URL`: Memgraph (none, with the reason).
//!
//! ```sh
//! DBINE_TEST_NEO4J_APOC_URL=neo4j:dbine-test-pass@localhost:17688 \
//!   cargo test -p dbine-driver-neo4j --test jobs -- --ignored
//! ```
//!
//! The APOC test makes its own `dbine_test_jobs_*` jobs and cancels them at
//! the end, also when an assertion fails.

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let (auth, hp) = url
        .rsplit_once('@')
        .map_or((None, url.as_str()), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':')?;
    let (user, pass) = auth
        .and_then(|a| a.split_once(':'))
        .map_or((None, None), |(u, p)| {
            (Some(u.to_string()), Some(p.to_string()))
        });
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: user,
        password: pass,
        ..Default::default()
    })
}

async fn open(driver: &str, cfg: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap()
        .connect(cfg, None)
        .await
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, q: &str) {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
}

/// Cancels every `dbine_test_jobs_*` APOC job still going.
async fn clean(cfg: &ConnectionConfig) {
    let mut s = open("neo4j", cfg).await;
    run(
        &mut s,
        "CALL apoc.periodic.list() YIELD name, done, cancelled
         WITH name WHERE name STARTS WITH 'dbine_test_jobs_' AND NOT done AND NOT cancelled
         CALL apoc.periodic.cancel(name) YIELD name AS c RETURN count(c)",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_apoc_jobs() {
    let Some(c) = cfg("neo4j", "DBINE_TEST_NEO4J_APOC_URL") else {
        eprintln!("DBINE_TEST_NEO4J_APOC_URL not set; skipping");
        return;
    };
    clean(&c).await;
    let cc = c.clone();
    let body = tokio::spawn(async move { apoc_body(&cc).await }).await;
    clean(&c).await;
    body.unwrap();
}

async fn apoc_body(c: &ConnectionConfig) {
    let d = dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == "neo4j")
        .unwrap();
    assert!(d.capabilities().jobs);
    let mut s = open("neo4j", c).await;
    run(
        &mut s,
        "CALL apoc.periodic.repeat('dbine_test_jobs_rep', 'RETURN 1', 3600) YIELD name RETURN name",
    )
    .await;
    run(
        &mut s,
        "CALL apoc.periodic.submit('dbine_test_jobs_slow', 'CALL apoc.util.sleep(60000)') YIELD name RETURN name",
    )
    .await;
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    assert!(list.notes[0].contains("memoria"));
    let rep = list
        .jobs
        .iter()
        .find(|j| j.name == "dbine_test_jobs_rep")
        .unwrap();
    assert_eq!(rep.id, "apoc:dbine_test_jobs_rep");
    assert_eq!(rep.kind, "apoc.periodic");
    assert_eq!(rep.schedule.as_deref(), Some("rate 3600s"));
    // A repeating job: APOC can only cancel it for good, so no stop.
    assert!(rep.enabled && !rep.running && !rep.can_stop);
    assert!(!rep.can_run && !rep.can_enable && !rep.has_history && !rep.has_script);
    let slow = list
        .jobs
        .iter()
        .find(|j| j.name == "dbine_test_jobs_slow")
        .unwrap();
    assert!(slow.running && slow.can_stop && slow.schedule.is_none());

    let det = s.job_detail(&rep.id).await.unwrap();
    assert!(det
        .properties
        .iter()
        .any(|p| p.label == "Cada (segundos)" && p.value == "3600"));
    assert!(det.properties.iter().any(|p| p.label == "Para cancelarlo"
        && p.value == "CALL apoc.periodic.cancel('dbine_test_jobs_rep')"));
    assert!(s
        .job_detail(&slow.id)
        .await
        .unwrap()
        .properties
        .iter()
        .any(|p| p.label == "Al detenerlo"));

    assert!(s.run_job(&rep.id).await.is_err());
    assert!(s.set_job_enabled(&rep.id, false).await.is_err());
    assert!(s.job_runs(&rep.id, 5).await.is_err());
    assert!(s.job_script(&rep.id).await.is_err());

    // Stop the running one-off job: APOC cancels it and drops it from its
    // list. The repeating one is refused and keeps going.
    s.stop_job(&slow.id).await.unwrap();
    assert!(s.stop_job(&rep.id).await.is_err());
    let left = s.jobs().await.unwrap();
    assert!(
        left.jobs
            .iter()
            .filter(|j| j.name == "dbine_test_jobs_slow")
            .all(|j| !j.enabled && !j.running),
        "{left:?}"
    );
    assert!(left
        .jobs
        .iter()
        .any(|j| j.name == "dbine_test_jobs_rep" && j.enabled));
    assert!(s.stop_job(&slow.id).await.is_err(), "already gone");

    for bad in ["x' OR 1=1 --", "dbine_test_jobs_rep", "apoc:", "apoc:a\nb"] {
        assert!(s.job_detail(bad).await.is_err(), "{bad}");
        assert!(s.stop_job(bad).await.is_err(), "{bad}");
    }
    // A name that would close the string is just a name nobody has.
    let e = s
        .stop_job("apoc:x') YIELD name CALL apoc.periodic.cancel('dbine_test_jobs_rep")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no existe"), "{e}");
    assert!(s
        .jobs()
        .await
        .unwrap()
        .jobs
        .iter()
        .any(|j| j.name == "dbine_test_jobs_rep" && j.enabled));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_without_apoc() {
    let Some(c) = cfg("neo4j", "DBINE_TEST_NEO4J_URL") else {
        eprintln!("DBINE_TEST_NEO4J_URL not set; skipping");
        return;
    };
    let mut s = open("neo4j", &c).await;
    let list = s.jobs().await.unwrap();
    assert!(list.jobs.is_empty());
    assert!(list.notes[0].contains("APOC"), "{list:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn memgraph_has_none() {
    let Some(c) = cfg("memgraph", "DBINE_TEST_MEMGRAPH_URL") else {
        eprintln!("DBINE_TEST_MEMGRAPH_URL not set; skipping");
        return;
    };
    let d = dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == "memgraph")
        .unwrap();
    assert!(!d.capabilities().jobs);
    let mut s = open("memgraph", &c).await;
    let list = s.jobs().await.unwrap();
    assert!(list.jobs.is_empty());
    assert!(list.notes[0].contains("Memgraph"), "{list:?}");
    assert!(s.job_detail("apoc:x").await.is_err());
}

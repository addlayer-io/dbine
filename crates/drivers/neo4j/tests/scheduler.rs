//! APOC's background jobs managed through the scheduler contract, against
//! real servers. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! (`user:pass@host:port`) and is skipped without it:
//!
//! - `DBINE_TEST_NEO4J_APOC_URL`: Neo4j 5 with APOC, e.g.
//!   `docker run -d --name dbine-test-neo4j-apoc -p 17688:7687
//!   -e NEO4J_AUTH=neo4j/dbine-test-pass -e NEO4J_PLUGINS='["apoc"]' neo4j:5`
//!   and `neo4j:dbine-test-pass@localhost:17688`;
//! - `DBINE_TEST_NEO4J_URL`: Neo4j without APOC (no kinds, a note).
//!
//! ```sh
//! DBINE_TEST_NEO4J_APOC_URL=neo4j:dbine-test-pass@localhost:17688 \
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 \
//!   cargo test -p dbine-driver-neo4j --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! The APOC test makes its own `dbine_test_sched_*` jobs and
//! `DbineTestSched*` nodes and removes them at the end, also when an
//! assertion fails.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, FormValues, QueryOutcome, Session};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn cfg(env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let (auth, hp) = url.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hp.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "neo4j".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn open(cfg: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == "neo4j")
        .unwrap()
        .connect(cfg, None)
        .await
        .unwrap()
}

async fn rows(s: &mut Box<dyn Session>, q: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(q, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    assert!(out.error.is_none(), "{q}: {:?}", out.error);
    out.results
        .iter()
        .rfind(|r| !r.columns.is_empty())
        .map(|r| r.rows.clone())
        .unwrap_or_default()
}

async fn count(s: &mut Box<dyn Session>, q: &str) -> i64 {
    rows(s, q)
        .await
        .first()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(0)
}

/// The test's jobs on the server, by name: (rate, done, cancelled).
async fn listed(s: &mut Box<dyn Session>) -> BTreeMap<String, (i64, bool, bool)> {
    rows(
        s,
        "CALL apoc.periodic.list() YIELD name, rate, done, cancelled
         WHERE name STARTS WITH 'dbine_test_sched_'
         RETURN name, rate, done, cancelled",
    )
    .await
    .into_iter()
    .map(|r| {
        (
            r[0].as_str().unwrap().to_string(),
            (
                r[1].as_i64().unwrap(),
                r[2].as_bool().unwrap(),
                r[3].as_bool().unwrap(),
            ),
        )
    })
    .collect()
}

/// Cancels the test's jobs and deletes its nodes; returns what is left.
async fn clean(cfg: &ConnectionConfig) -> usize {
    let mut s = open(cfg).await;
    rows(
        &mut s,
        "CALL apoc.periodic.list() YIELD name, done, cancelled
         WITH name WHERE name STARTS WITH 'dbine_test_sched_' AND NOT cancelled
         CALL apoc.periodic.cancel(name) YIELD name AS c RETURN count(c)",
    )
    .await;
    // A run that started before the cancel may still write: wait it out.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    rows(
        &mut s,
        "MATCH (n) WHERE n:DbineTestSched OR n:DbineTestSchedCounter DETACH DELETE n",
    )
    .await;
    let left = listed(&mut s)
        .await
        .values()
        .filter(|(_, done, cancelled)| !done && !cancelled)
        .count();
    let nodes = count(
        &mut s,
        "MATCH (n) WHERE n:DbineTestSched OR n:DbineTestSchedCounter RETURN count(n)",
    )
    .await;
    left + nodes as usize
}

fn values(v: Value) -> FormValues {
    FormValues {
        values: serde_json::from_value(v).unwrap(),
        lists: BTreeMap::new(),
    }
}

async fn wait_tag(s: &mut Box<dyn Session>, tag: &str) {
    let start = Instant::now();
    while count(
        s,
        &format!("MATCH (n:DbineTestSched {{tag: '{tag}'}}) RETURN count(n)"),
    )
    .await
        == 0
    {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "no run wrote {tag}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

const REP: &str = "dbine_test_sched_rep";
const REP3: &str = "dbine_test_sched_rep3";
const ONCE: &str = "dbine_test_sched_once";
const DOWN: &str = "dbine_test_sched_down";
const STMT: &str = "CREATE (:DbineTestSched {tag: $tag, at: datetime()})";

async fn apoc_body(c: ConnectionConfig) {
    for d in dbine_driver_neo4j::drivers() {
        assert_eq!(d.capabilities().scheduler, d.info().id == "neo4j");
    }
    let mut s = open(&c).await;

    let info = s.scheduler_info().await.expect("info");
    assert_eq!(info.kinds.len(), 1);
    let k = &info.kinds[0];
    assert_eq!((k.key.as_str(), k.label.as_str()), ("job", "Jobs de APOC"));
    assert!(k.can_create && k.can_edit && k.can_delete);
    assert!(!info.logs && info.notes.iter().any(|n| n.contains("reinicio")));

    let f = s.scheduler_form("job", None).await.expect("new form");
    assert_eq!(f.values["type"], json!("repeat"));

    // Create a repeating job with a parameter: it runs.
    let new = values(json!({
        "name": REP, "type": "repeat", "rate": 1, "statement": STMT,
        "params": "{\"tag\": \"rep\"}"
    }));
    let script = s.scheduler_script("job", None, &new).await.expect("script");
    assert!(script.starts_with("// Job de APOC"), "{script}");
    assert!(script.contains("CALL apoc.periodic.repeat('dbine_test_sched_rep', 'CREATE (:DbineTestSched {tag: $tag, at: datetime()})', 1, {params: {tag: 'rep'}});"), "{script}");
    let id = s
        .scheduler_save("job", None, &new)
        .await
        .expect("create")
        .unwrap();
    assert_eq!(id, format!("apoc:{REP}"));
    assert_eq!(listed(&mut s).await[REP], (1, false, false));
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));
    let items = s.scheduler_items("job").await.unwrap();
    let item = items.items.iter().find(|i| i.id == id).expect("item");
    assert_eq!(item.enabled, Some(true));
    wait_tag(&mut s, "rep").await;
    // The same name while it runs is refused.
    let e = s.scheduler_save("job", None, &new).await.unwrap_err();
    assert!(e.to_string().contains("Ya hay"), "{e}");

    // Its form: what APOC lists; the statement to type again.
    let f = s.scheduler_form("job", Some(&id)).await.expect("form");
    assert_eq!(f.values["rate"], json!(1));
    assert_eq!(f.values["type"], json!("repeat"));
    assert_eq!(f.values["statement"], json!(""));

    // Edit: rate and parameter; still one job.
    let edited = values(json!({
        "name": REP, "type": "repeat", "rate": 2, "statement": STMT,
        "params": {"tag": "rep2"}
    }));
    s.scheduler_save("job", Some(&id), &edited)
        .await
        .expect("edit");
    assert_eq!(listed(&mut s).await[REP], (2, false, false));
    wait_tag(&mut s, "rep2").await;

    // Rename: the new one scheduled, the old one cancelled.
    let renamed = values(json!({
        "name": REP3, "type": "repeat", "rate": 3600, "statement": STMT,
        "params": {"tag": "rep3"}
    }));
    let script = s
        .scheduler_script("job", Some(&id), &renamed)
        .await
        .unwrap();
    assert!(
        script.contains("CALL apoc.periodic.cancel('dbine_test_sched_rep');"),
        "{script}"
    );
    let id3 = s
        .scheduler_save("job", Some(&id), &renamed)
        .await
        .expect("rename")
        .unwrap();
    assert_eq!(id3, format!("apoc:{REP3}"));
    let l = listed(&mut s).await;
    assert_eq!(l[REP3], (3600, false, false));
    assert!(!l.get(REP).is_some_and(|(_, d, c)| !d && !c), "{l:?}");

    // A one-off job with a parameter.
    let once = values(json!({
        "name": ONCE, "type": "submit", "statement": STMT, "params": {"tag": "once"}
    }));
    assert_eq!(
        s.scheduler_save("job", None, &once).await.expect("submit"),
        Some(format!("apoc:{ONCE}"))
    );
    wait_tag(&mut s, "once").await;

    // A countdown: runs until its statement returns 0. During its first
    // run APOC lists it as one (a delay, no rate).
    rows(&mut s, "CREATE (:DbineTestSchedCounter {n: 3})").await;
    let down = values(json!({
        "name": DOWN, "type": "countdown", "rate": 1,
        "statement": "MATCH (c:DbineTestSchedCounter) CALL apoc.util.sleep(2000) SET c.n = c.n - 1 RETURN c.n"
    }));
    let did = s
        .scheduler_save("job", None, &down)
        .await
        .expect("countdown")
        .unwrap();
    let f = s.scheduler_form("job", Some(&did)).await.expect("form");
    assert_eq!(f.values["type"], json!("countdown"), "{:?}", f.values);
    assert_eq!(f.values["rate"], json!(1));
    // Its own name again is refused: its pending run would replace it.
    let e = s
        .scheduler_save("job", Some(&did), &down)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("otro nombre"), "{e}");
    // The delete script says what APOC's cancel can't do, and indeed a
    // cancel during its first run doesn't stop it: it still counts to 0.
    let script = s.scheduler_delete_script("job", &did).await.unwrap();
    assert!(
        script.contains("no puede cancelar una cuenta regresiva"),
        "{script}"
    );
    s.scheduler_delete("job", &did).await.expect("cancel");
    let start = Instant::now();
    while count(&mut s, "MATCH (c:DbineTestSchedCounter) RETURN c.n").await > 0 {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the countdown stopped at the cancel"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Refused: a statement APOC can't parse, a bad rate, bad ids, kinds.
    let e = s
        .scheduler_save(
            "job",
            None,
            &values(json!({"name": "dbine_test_sched_bad", "rate": 5, "statement": "MATCH (n RETURN n"})),
        )
        .await
        .unwrap_err();
    assert!(!e.to_string().is_empty());
    assert!(!listed(&mut s).await.contains_key("dbine_test_sched_bad"));
    assert!(s
        .scheduler_save(
            "job",
            None,
            &values(json!({"name": "dbine_test_sched_bad", "rate": 0, "statement": "RETURN 1"}))
        )
        .await
        .is_err());
    for bad in [
        "apoc:dbine_test_sched_nope",
        "dbine_test_sched_rep3",
        "apoc:",
        "apoc:x') YIELD name RETURN 1 //",
        "",
    ] {
        assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad:?}");
        assert!(s.scheduler_delete("job", bad).await.is_err(), "{bad:?}");
    }
    assert!(s.scheduler_items("schedule").await.is_err());
    assert!(s.run_job_from(&id3, "x").await.is_err());
    assert!(s.clear_job_history(&id3).await.is_err());
    assert!(s.scheduler_logs().await.is_err());

    // A read-only connection sees, changes nothing.
    let mut ro = ReadOnlySession::new(open(&c).await);
    assert!(ro.scheduler_form("job", Some(&id3)).await.is_ok());
    assert!(ro.scheduler_delete_script("job", &id3).await.is_ok());
    assert!(ro.scheduler_delete("job", &id3).await.is_err());
    assert!(ro.scheduler_save("job", None, &once).await.is_err());
    assert_eq!(listed(&mut s).await[REP3], (3600, false, false));

    // Delete: cancelled and gone from the list.
    let script = s.scheduler_delete_script("job", &id3).await.unwrap();
    assert!(script.contains("CALL apoc.periodic.cancel('dbine_test_sched_rep3');"));
    s.scheduler_delete("job", &id3).await.expect("delete");
    assert!(!listed(&mut s)
        .await
        .get(REP3)
        .is_some_and(|(_, d, c)| !d && !c));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_apoc_jobs_in_the_designer() {
    let Some(c) = cfg("DBINE_TEST_NEO4J_APOC_URL") else {
        eprintln!("DBINE_TEST_NEO4J_APOC_URL not set; skipping");
        return;
    };
    clean(&c).await;
    let r = tokio::spawn(apoc_body(c.clone())).await;
    let left = clean(&c).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
    assert_eq!(left, 0, "jobs or nodes left behind");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_without_apoc_says_so() {
    let Some(c) = cfg("DBINE_TEST_NEO4J_URL") else {
        eprintln!("DBINE_TEST_NEO4J_URL not set; skipping");
        return;
    };
    let mut s = open(&c).await;
    let info = s.scheduler_info().await.expect("info");
    assert!(info.kinds.is_empty());
    assert!(info.notes[0].contains("APOC"), "{:?}", info.notes);
    let items = s.scheduler_items("job").await.expect("items");
    assert!(items.items.is_empty() && items.notes[0].contains("APOC"));
    assert!(s.scheduler_form("job", None).await.is_err());
    let v = values(json!({"name": "dbine_test_sched_x", "rate": 5, "statement": "RETURN 1"}));
    assert!(s.scheduler_save("job", None, &v).await.is_err());
}

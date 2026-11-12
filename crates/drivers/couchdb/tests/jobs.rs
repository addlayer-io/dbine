//! Server jobs (replications) against a real server (see
//! tests/integration.rs):
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test jobs -- --ignored
//! ```
//!
//! The replications run inside the server, so their URLs point at the
//! server's own port 5984. The test makes its own databases, `_replicator`
//! documents and non-admin user (`dbine_test_jobs_*`) and deletes them at
//! the end, also when an assertion fails; it makes `_replicator` itself only
//! when the server lacks it, and then drops it too.

use dbine_driver::{ConnectionConfig, JobOutcome, Session};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const SRC: &str = "dbine_test_jobs_src";
const DST: &str = "dbine_test_jobs_dst";
const ONCE: &str = "dbine_test_jobs_once";
const LIVE: &str = "dbine_test_jobs_live";
/// A user that isn't a server admin.
const USER: &str = "dbine_test_jobs_user";

fn cfg() -> Option<(ConnectionConfig, String, String, String)> {
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").ok()?;
    let rest = url.strip_prefix("http://")?;
    let (auth, host) = rest.split_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (h, p) = host.trim_end_matches('/').split_once(':')?;
    let c = ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    Some((
        c,
        format!("http://{host}").trim_end_matches('/').to_string(),
        user.into(),
        pass.into(),
    ))
}

struct Http {
    c: reqwest::Client,
    base: String,
    user: String,
    pass: String,
}

impl Http {
    async fn send(&self, m: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut rq = self
            .c
            .request(m, format!("{}{path}", self.base))
            .basic_auth(&self.user, Some(&self.pass));
        if let Some(b) = body {
            rq = rq.json(&b);
        }
        let r = rq.send().await.expect("http");
        let st = r.status().as_u16();
        (st, r.json().await.unwrap_or(Value::Null))
    }
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_couchdb::drivers()[0]
        .connect(c, None)
        .await
        .expect("connect")
}

/// The internal URL of a test database, with the credentials in it.
fn inner(h: &Http, db: &str) -> String {
    format!("http://{}:{}@127.0.0.1:5984/{db}", h.user, h.pass)
}

async fn cleanup(h: &Http, made_replicator: bool) {
    use reqwest::Method;
    for doc in [ONCE, LIVE] {
        let (st, d) = h
            .send(Method::GET, &format!("/_replicator/{doc}"), None)
            .await;
        if st == 200 {
            let rev = d["_rev"].as_str().unwrap_or_default().to_string();
            h.send(
                Method::DELETE,
                &format!("/_replicator/{doc}?rev={rev}"),
                None,
            )
            .await;
        }
    }
    let (_, jobs) = h.send(Method::GET, "/_scheduler/jobs", None).await;
    for j in jobs["jobs"].as_array().into_iter().flatten() {
        if j["database"].is_null() && j["source"].as_str().is_some_and(|s| s.contains(SRC)) {
            h.send(
                Method::POST,
                "/_replicate",
                Some(json!({"replication_id": j["id"], "cancel": true})),
            )
            .await;
        }
    }
    for db in [SRC, DST] {
        h.send(Method::DELETE, &format!("/{db}"), None).await;
    }
    let user_doc = format!("/_users/org.couchdb.user:{USER}");
    let (st, d) = h.send(Method::GET, &user_doc, None).await;
    if st == 200 {
        let rev = d["_rev"].as_str().unwrap_or_default().to_string();
        h.send(Method::DELETE, &format!("{user_doc}?rev={rev}"), None)
            .await;
    }
    if made_replicator {
        h.send(Method::DELETE, "/_replicator", None).await;
    }
}

async fn wait_for<F: Fn(&dbine_driver::JobList) -> bool>(
    s: &mut Box<dyn Session>,
    what: &str,
    ok: F,
) -> dbine_driver::JobList {
    let start = Instant::now();
    loop {
        let list = s.jobs().await.expect("jobs");
        if ok(&list) {
            return list;
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "{what}: {:#?}",
            list.jobs
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn find<'a>(l: &'a dbine_driver::JobList, id: &str) -> Option<&'a dbine_driver::ServerJob> {
    l.jobs.iter().find(|j| j.id == id)
}

async fn body(c: ConnectionConfig, h: Http) {
    use reqwest::Method;
    let d = &dbine_driver_couchdb::drivers()[0];
    assert!(d.capabilities().jobs);
    for db in [SRC, DST] {
        h.send(Method::PUT, &format!("/{db}"), None).await;
    }
    h.send(Method::PUT, &format!("/{SRC}/doc1"), Some(json!({"a": 1})))
        .await;
    let (st, _) = h
        .send(
            Method::PUT,
            &format!("/_replicator/{ONCE}"),
            Some(json!({"source": inner(&h, SRC), "target": inner(&h, DST)})),
        )
        .await;
    assert_eq!(st, 201);
    let (st, _) = h
        .send(
            Method::PUT,
            &format!("/_replicator/{LIVE}"),
            Some(json!({
                "source": {"url": format!("http://127.0.0.1:5984/{SRC}"),
                           "auth": {"basic": {"username": h.user, "password": h.pass}}},
                "target": inner(&h, DST), "continuous": true, "create_target": false
            })),
        )
        .await;
    assert_eq!(st, 201);

    let once_id = format!("doc:_replicator:{ONCE}");
    let live_id = format!("doc:_replicator:{LIVE}");
    let mut s = open(&c).await;

    // Both listed: the one-shot one finishes, the continuous one runs.
    let list = wait_for(&mut s, "replications never settled", |l| {
        find(l, &once_id).is_some_and(|j| j.last_outcome == Some(JobOutcome::Succeeded))
            && find(l, &live_id).is_some_and(|j| j.running)
    })
    .await;
    let once = find(&list, &once_id).unwrap();
    assert_eq!(once.kind, "_replicator");
    assert_eq!(once.group.as_deref(), Some("_replicator"));
    assert!(once.can_run && !once.can_stop && once.has_history && once.has_script);
    let live = find(&list, &live_id).unwrap();
    assert!(live.can_stop && !live.can_run && !live.can_enable);
    assert!(
        !live.description.as_deref().unwrap_or("").contains(&h.pass),
        "no password in the list"
    );

    // Detail.
    let det = s.job_detail(&live_id).await.expect("detail");
    assert!(det
        .properties
        .iter()
        .any(|p| p.label == "Continua" && p.value == "sí"));
    assert!(det.notes.iter().any(|n| n.contains("borra el documento")));

    // History: the finished one's final run, the live one's start.
    let runs = s.job_runs(&once_id, 10).await.expect("runs");
    assert!(
        runs.iter().any(|r| r.outcome == JobOutcome::Succeeded),
        "{runs:?}"
    );
    let runs = s.job_runs(&live_id, 10).await.expect("runs");
    assert_eq!(
        runs.first().map(|r| r.outcome),
        Some(JobOutcome::Running),
        "{runs:?}"
    );

    // Run now restarts the finished one: a new revision, finished again.
    let (_, before) = h
        .send(Method::GET, &format!("/_replicator/{ONCE}"), None)
        .await;
    s.run_job(&once_id).await.expect("run now");
    let (_, after) = h
        .send(Method::GET, &format!("/_replicator/{ONCE}"), None)
        .await;
    assert_ne!(before["_rev"], after["_rev"]);
    wait_for(&mut s, "the restarted one never finished", |l| {
        find(l, &once_id).is_some_and(|j| j.last_outcome == Some(JobOutcome::Succeeded))
    })
    .await;
    // A running one isn't run again.
    assert!(s.run_job(&live_id).await.is_err());

    // Script: the document as a PUT, credentials hidden.
    let script = s.job_script(&live_id).await.expect("script");
    assert!(script.contains(&format!("PUT /_replicator/{LIVE}")));
    assert!(
        !script.contains(&h.pass) && script.contains("***"),
        "{script}"
    );

    // Stop deletes the document.
    s.stop_job(&live_id).await.expect("stop");
    let (st, _) = h
        .send(Method::GET, &format!("/_replicator/{LIVE}"), None)
        .await;
    assert_eq!(st, 404);
    wait_for(&mut s, "the stopped one is still listed", |l| {
        find(l, &live_id).is_none()
    })
    .await;

    // A transient replication: listed, with history, stopped by cancel.
    let (st, rep) = h
        .send(
            Method::POST,
            "/_replicate",
            Some(json!({"source": inner(&h, SRC), "target": inner(&h, DST), "continuous": true})),
        )
        .await;
    assert_eq!(st, 202, "{rep}");
    let rep_id = format!("rep:{}", rep["_local_id"].as_str().expect("replication id"));
    let list = wait_for(&mut s, "the transient one isn't listed", |l| {
        find(l, &rep_id).is_some()
    })
    .await;
    let t = find(&list, &rep_id).unwrap();
    assert_eq!(t.kind, "_replicate");
    assert!(t.can_stop && !t.can_run);
    assert!(s.job_detail(&rep_id).await.is_ok());
    assert!(s
        .job_script(&rep_id)
        .await
        .unwrap()
        .contains("POST /_replicate"));
    assert!(s.job_runs(&rep_id, 5).await.is_ok());
    s.stop_job(&rep_id).await.expect("cancel");
    wait_for(&mut s, "the cancelled one is still listed", |l| {
        find(l, &rep_id).is_none()
    })
    .await;

    // Malicious or unknown ids are refused before any request.
    for bad in [
        "doc:_users:org.couchdb.user:admin",
        "doc:_replicator:../_users/x",
        "doc:_replicator:_design/x",
        "doc:_replicator:dbine_test_jobs_nope",
        "doc:_replicator:..",
        "doc:_replicator:.",
        "rep:../../_users",
        "rep:0123",
        "",
    ] {
        assert!(s.job_detail(bad).await.is_err(), "{bad:?}");
        assert!(s.stop_job(bad).await.is_err(), "{bad:?}");
        assert!(s.run_job(bad).await.is_err(), "{bad:?}");
        assert!(s.job_script(bad).await.is_err(), "{bad:?}");
    }
    let (st, _) = h
        .send(Method::GET, &format!("/_replicator/{ONCE}"), None)
        .await;
    assert_eq!(st, 200, "the bad ids touched nothing");

    // A user who isn't a server admin gets an empty list and a note.
    let (st, _) = h
        .send(
            Method::PUT,
            &format!("/_users/org.couchdb.user:{USER}"),
            Some(json!({"name": USER, "password": "secreto1", "roles": [], "type": "user"})),
        )
        .await;
    assert_eq!(st, 201, "user");
    let mut plain = c.clone();
    plain.username = Some(USER.into());
    plain.password = Some("secreto1".into());
    let mut u = open(&plain).await;
    let list = u.jobs().await.expect("a note, not an error");
    assert!(list.jobs.is_empty(), "{list:?}");
    assert!(
        list.notes.iter().any(|n| n.contains("administrador")),
        "{list:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn replications_as_jobs() {
    let Some((c, base, user, pass)) = cfg() else {
        eprintln!("DBINE_TEST_COUCHDB_URL not set; skipping");
        return;
    };
    let h = Http {
        c: reqwest::Client::new(),
        base,
        user,
        pass,
    };
    let (st, _) = h.send(reqwest::Method::GET, "/_replicator", None).await;
    let made_replicator = st == 404;
    if made_replicator {
        h.send(reqwest::Method::PUT, "/_replicator", None).await;
    }
    cleanup(&h, false).await;
    let h2 = Http {
        c: reqwest::Client::new(),
        base: h.base.clone(),
        user: h.user.clone(),
        pass: h.pass.clone(),
    };
    let r = tokio::spawn(body(c, h2)).await;
    cleanup(&h, made_replicator).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

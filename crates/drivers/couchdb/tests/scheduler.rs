//! CouchDB's replications and the replicator's settings managed through the
//! scheduler contract, against a real server (see tests/integration.rs):
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! The replications run inside the server, so their URLs point at its own
//! port 5984. Everything the test makes is named `dbine_test_sched_*` (the
//! one replication saved without an id is found by its source) and deleted
//! at the end, also when an assertion fails: documents, transient
//! replications, databases, and `_replicator` itself when the server
//! didn't have it; the replicator's settings are put back as they were.

use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, Session, SECRET_MASK};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const SRC: &str = "dbine_test_sched_src";
const DST: &str = "dbine_test_sched_dst";
const DST2: &str = "dbine_test_sched_dst2";
const DST3: &str = "dbine_test_sched_dst3";
const DOC_A: &str = "dbine_test_sched_a";
/// The setting the test changes.
const KEY: &str = "max_history";

fn cfg() -> Option<(ConnectionConfig, Http)> {
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
        Http {
            c: reqwest::Client::new(),
            base: format!("http://{}", host.trim_end_matches('/')),
            user: user.into(),
            pass: pass.into(),
        },
    ))
}

#[derive(Clone)]
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

fn inner(db: &str) -> String {
    format!("http://127.0.0.1:5984/{db}")
}

/// The test's documents in `_replicator`: by id, or by a source that names
/// a test database.
async fn our_docs(h: &Http) -> Vec<(String, String)> {
    let (st, all) = h
        .send(
            reqwest::Method::GET,
            "/_replicator/_all_docs?include_docs=true",
            None,
        )
        .await;
    if st != 200 {
        return Vec::new();
    }
    all["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| {
            let id = r["id"].as_str().unwrap_or_default();
            id.starts_with("dbine_test_sched_") || r["doc"]["source"].to_string().contains(SRC)
        })
        .map(|r| {
            (
                r["id"].as_str().unwrap_or_default().to_string(),
                r["value"]["rev"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Deletes what the test made; what is left.
async fn cleanup(h: &Http, made_replicator: bool, setting: Option<String>) -> Vec<String> {
    use reqwest::Method;
    for (id, rev) in our_docs(h).await {
        h.send(
            Method::DELETE,
            &format!("/_replicator/{id}?rev={rev}"),
            None,
        )
        .await;
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
    for db in [SRC, DST, DST2, DST3] {
        h.send(Method::DELETE, &format!("/{db}"), None).await;
    }
    let key = format!("/_node/_local/_config/replicator/{KEY}");
    match setting {
        Some(v) => {
            h.send(Method::PUT, &key, Some(json!(v))).await;
        }
        None => {
            h.send(Method::DELETE, &key, None).await;
        }
    }
    let mut left: Vec<String> = our_docs(h).await.into_iter().map(|(id, _)| id).collect();
    if made_replicator && left.is_empty() {
        h.send(Method::DELETE, "/_replicator", None).await;
    }
    let (_, dbs) = h.send(Method::GET, "/_all_dbs", None).await;
    for d in dbs
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if d.starts_with("dbine_test_sched_") || (made_replicator && d == "_replicator") {
            left.push(d.to_string());
        }
    }
    left
}

/// What the designer would send back for a form as it came.
fn values_of(form: &ObjectForm) -> FormValues {
    let mut values = form.values.clone();
    for f in form.sections.iter().flat_map(|s| s.fields.iter()) {
        if let Some(d) = &f.default {
            values.entry(f.key.clone()).or_insert_with(|| d.clone());
        }
    }
    FormValues {
        values,
        lists: Default::default(),
    }
}

fn set(v: &mut FormValues, k: &str, x: Value) {
    v.values.insert(k.into(), x);
}

async fn save(s: &mut Box<dyn Session>, kind: &str, id: Option<&str>, v: &FormValues) -> String {
    let script = s.scheduler_script(kind, id, v).await.expect("script");
    eprintln!("--- {kind} {id:?}\n{script}");
    assert!(
        !script.contains("\"secret\""),
        "a password in the script: {script}"
    );
    let r = s.scheduler_save(kind, id, v).await;
    r.unwrap_or_else(|e| panic!("save {kind}: {e:?}\n{script}"))
        .expect("an id")
}

async fn wait_state(h: &Http, doc: &str, states: &[&str]) -> Value {
    let start = Instant::now();
    loop {
        let (st, d) = h
            .send(
                reqwest::Method::GET,
                &format!("/_scheduler/docs/_replicator/{doc}"),
                None,
            )
            .await;
        if st == 200 && states.iter().any(|s| d["state"] == *s) {
            return d;
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "{doc} never reached {states:?}: {d}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn doc(h: &Http, id: &str) -> Value {
    let (st, d) = h
        .send(reqwest::Method::GET, &format!("/_replicator/{id}"), None)
        .await;
    assert_eq!(st, 200, "{id}: {d}");
    d
}

async fn body(c: ConnectionConfig, h: Http) {
    use reqwest::Method;
    let mut s = open(&c).await;
    h.send(Method::PUT, &format!("/{SRC}"), None).await;
    for (id, tipo) in [("d1", "a"), ("d2", "b"), ("d3", "a")] {
        h.send(
            Method::PUT,
            &format!("/{SRC}/{id}"),
            Some(json!({"tipo": tipo})),
        )
        .await;
    }
    h.send(
        Method::PUT,
        &format!("/{SRC}/_design/app"),
        Some(json!({"views": {"a": {"map": "function(d){ if (d.tipo == 'a') emit(d._id); }"}}})),
    )
    .await;

    let info = s.scheduler_info().await.expect("info");
    let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "settings"]);
    // One step, restarted by "run now"; no history to clear; no actions.
    let job = &info.kinds[0];
    assert!(
        !job.run_from_step && !job.clear_history && job.actions.is_empty(),
        "{job:?}"
    );

    // A one-shot replication with credentials and a selector, into a target
    // it creates.
    let f = s.scheduler_form("job", None).await.expect("new form");
    assert!(f.choices["replicator_db"]
        .iter()
        .any(|(d, _)| d == "_replicator"));
    let mut v = values_of(&f);
    set(&mut v, "doc_id", json!(DOC_A));
    set(&mut v, "source_url", json!(inner(SRC)));
    set(&mut v, "source_user", json!(h.user));
    set(&mut v, "source_password", json!(h.pass));
    set(&mut v, "target_url", json!(inner(DST)));
    set(&mut v, "target_user", json!(h.user));
    set(&mut v, "target_password", json!(h.pass));
    set(&mut v, "create_target", json!(true));
    set(&mut v, "create_target_q", json!(1));
    set(&mut v, "filter_type", json!("selector"));
    set(&mut v, "selector", json!("{\"tipo\": \"a\"}"));
    let script = s.scheduler_script("job", None, &v).await.unwrap();
    assert!(
        script.contains(SECRET_MASK) && !script.contains(&format!("\"{}\"", h.pass)),
        "{script}"
    );
    let id = save(&mut s, "job", None, &v).await;
    assert_eq!(id, format!("doc:_replicator:{DOC_A}"));
    let d = doc(&h, DOC_A).await;
    assert_eq!(d["source"]["auth"]["basic"]["password"], h.pass.as_str());
    assert_eq!(d["selector"], json!({"tipo": "a"}));
    assert_eq!(d["create_target_params"], json!({"q": 1}));
    wait_state(&h, DOC_A, &["completed"]).await;
    let (_, dst) = h
        .send(Method::GET, &format!("/{DST}/_all_docs"), None)
        .await;
    let ids: Vec<&str> = dst["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert_eq!(ids, ["d1", "d3"]);
    // The tree's job rows are the jobs' ids.
    let items = s.scheduler_items("job").await.unwrap();
    assert!(items.items.iter().any(|i| i.id == id));
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));

    // Run it again: a new revision, done again. It has one step: starting
    // at it is refused, as the kind's flag says.
    let rev = doc(&h, DOC_A).await["_rev"].clone();
    for step in ["Replicación", "Otro"] {
        match s.run_job_from(&id, step).await {
            Err(Error::Unsupported(m)) => assert!(m.contains("Ejecutar ahora"), "{m}"),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(doc(&h, DOC_A).await["_rev"], rev);
    s.run_job(&id).await.expect("run it again");
    assert_ne!(doc(&h, DOC_A).await["_rev"], rev);
    wait_state(&h, DOC_A, &["completed"]).await;
    assert!(matches!(
        s.clear_job_history(&id).await,
        Err(Error::Unsupported(_))
    ));

    // Its form: no password, the URL clean, the status.
    let f = s.scheduler_form("job", Some(&id)).await.expect("form");
    assert!(!f.read_only);
    assert!(f.values["source_password"].is_null());
    assert_eq!(f.values["source_auth"], "basic");
    assert_eq!(f.values["source_url"], inner(SRC).as_str());
    assert_eq!(f.values["source_user"], h.user.as_str());
    assert_eq!(f.values["filter_type"], "selector");
    assert_eq!(f.values["owner"], h.user.as_str());
    let shown = serde_json::to_string(&f.values).unwrap();
    assert!(!shown.contains(&format!("\"{}\"", h.pass)), "{shown}");
    // Unchanged: nothing to save.
    let v = values_of(&f);
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(script.contains("no hay cambios"), "{script}");
    let rev = doc(&h, DOC_A).await["_rev"].clone();
    assert_eq!(
        s.scheduler_save("job", Some(&id), &v)
            .await
            .unwrap()
            .as_deref(),
        Some(id.as_str())
    );
    assert_eq!(doc(&h, DOC_A).await["_rev"], rev);

    // Edit: continuous, two workers, by ids, another target; the passwords
    // stay without being typed.
    let mut v = values_of(&f);
    set(&mut v, "continuous", json!(true));
    set(&mut v, "worker_processes", json!(2));
    set(&mut v, "filter_type", json!("doc_ids"));
    set(&mut v, "doc_ids", json!("d2\nd3"));
    set(&mut v, "target_url", json!(inner(DST2)));
    save(&mut s, "job", Some(&id), &v).await;
    let d = doc(&h, DOC_A).await;
    assert_eq!(d["continuous"], true);
    assert_eq!(d["worker_processes"], 2);
    assert_eq!(d["doc_ids"], json!(["d2", "d3"]));
    assert!(d.get("selector").is_none());
    assert_eq!(d["target"]["url"], inner(DST2).as_str());
    assert_eq!(d["target"]["auth"]["basic"]["password"], h.pass.as_str());
    assert_eq!(d["source"]["auth"]["basic"]["password"], h.pass.as_str());
    assert_eq!(d["owner"], h.user.as_str());
    wait_state(&h, DOC_A, &["running"]).await;
    // A running one doesn't start again.
    assert!(s.run_job(&id).await.is_err());

    // One without an id, through a view, with a header.
    let f = s.scheduler_form("job", None).await.unwrap();
    let mut v = values_of(&f);
    set(&mut v, "source_url", json!(inner(SRC)));
    set(&mut v, "source_user", json!(h.user));
    set(&mut v, "source_password", json!(h.pass));
    set(&mut v, "target_url", json!(inner(DST3)));
    set(&mut v, "target_user", json!(h.user));
    set(&mut v, "target_password", json!(h.pass));
    set(&mut v, "target_headers", json!("{\"X-Dbine\": \"test\"}"));
    set(&mut v, "create_target", json!(true));
    set(&mut v, "filter_type", json!("view"));
    set(&mut v, "view_name", json!("app/a"));
    let id_b = save(&mut s, "job", None, &v).await;
    let doc_b = id_b
        .strip_prefix("doc:_replicator:")
        .expect("a document")
        .to_string();
    let d = doc(&h, &doc_b).await;
    assert_eq!(d["filter"], "_view");
    assert_eq!(d["query_params"], json!({"view": "app/a"}));
    assert_eq!(d["target"]["headers"], json!({"X-Dbine": "test"}));
    wait_state(&h, &doc_b, &["completed"]).await;
    let (_, dst3) = h
        .send(Method::GET, &format!("/{DST3}/_all_docs"), None)
        .await;
    assert_eq!(dst3["total_rows"], 2, "{dst3}");

    // Its target authenticates with a token instead: the header comes from
    // the Token field, the user and password go, the other header stays.
    let f = s.scheduler_form("job", Some(&id_b)).await.unwrap();
    assert_eq!(f.values["target_auth"], "basic");
    let mut v = values_of(&f);
    set(&mut v, "target_auth", json!("bearer"));
    set(&mut v, "target_token", json!("t0k-dbine"));
    let script = s.scheduler_script("job", Some(&id_b), &v).await.unwrap();
    assert!(!script.contains("t0k-dbine"), "{script}");
    save(&mut s, "job", Some(&id_b), &v).await;
    let d = doc(&h, &doc_b).await;
    assert_eq!(
        d["target"]["headers"],
        json!({"X-Dbine": "test", "Authorization": "Bearer t0k-dbine"})
    );
    assert!(d["target"].get("auth").is_none(), "{d}");
    let f = s.scheduler_form("job", Some(&id_b)).await.unwrap();
    assert_eq!(f.values["target_auth"], "bearer");
    assert!(f.values["target_token"].is_null());
    assert!(!f.values["target_headers"]
        .as_str()
        .unwrap()
        .contains("Authorization"));
    let script = s
        .scheduler_script("job", Some(&id_b), &values_of(&f))
        .await
        .unwrap();
    assert!(script.contains("no hay cambios"), "{script}");
    // And back to its user, the password typed again: the token goes.
    let mut v = values_of(&f);
    set(&mut v, "target_auth", json!("basic"));
    set(&mut v, "target_user", json!(h.user));
    set(&mut v, "target_password", json!(h.pass));
    save(&mut s, "job", Some(&id_b), &v).await;
    let d = doc(&h, &doc_b).await;
    assert_eq!(d["target"]["headers"], json!({"X-Dbine": "test"}));
    assert_eq!(d["target"]["auth"]["basic"]["password"], h.pass.as_str());

    // A transient replication: shown read-only, deleted = cancelled.
    let (st, r) = h
        .send(
            Method::POST,
            "/_replicate",
            Some(json!({"source": {"url": inner(SRC), "auth": {"basic": {"username": h.user, "password": h.pass}}},
                        "target": {"url": inner(DST), "auth": {"basic": {"username": h.user, "password": h.pass}}},
                        "continuous": true})),
        )
        .await;
    assert!(st < 300, "{r}");
    let rep = format!("rep:{}", r["_local_id"].as_str().unwrap());
    let start = Instant::now();
    while !s
        .scheduler_items("job")
        .await
        .unwrap()
        .items
        .iter()
        .any(|i| i.id == rep)
    {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the transient replication isn't listed"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let f = s.scheduler_form("job", Some(&rep)).await.unwrap();
    assert!(f.read_only && f.values["continuous"] == true);
    // The tree offers no edit on it, only its cancel.
    let it = s
        .scheduler_items("job")
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id == rep)
        .unwrap();
    assert_eq!((it.can_edit, it.can_delete), (Some(false), None));
    assert!(s
        .scheduler_save("job", Some(&rep), &values_of(&f))
        .await
        .is_err());
    let script = s.scheduler_delete_script("job", &rep).await.unwrap();
    assert!(
        script.contains("POST /_replicate") && script.contains("\"cancel\": true"),
        "{script}"
    );
    s.scheduler_delete("job", &rep).await.expect("cancel");
    assert!(!s
        .scheduler_items("job")
        .await
        .unwrap()
        .items
        .iter()
        .any(|i| i.id == rep));

    // Delete the documents.
    for i in [&id, &id_b] {
        let script = s.scheduler_delete_script("job", i).await.unwrap();
        assert!(
            script.contains("DELETE /_replicator/") && script.contains("?rev="),
            "{script}"
        );
        s.scheduler_delete("job", i).await.expect("delete");
    }
    for d in [DOC_A, doc_b.as_str()] {
        let (st, _) = h
            .send(Method::GET, &format!("/_replicator/{d}"), None)
            .await;
        assert_eq!(st, 404, "{d} is gone");
    }

    // The replicator's settings of the node.
    let nodes = s.scheduler_items("settings").await.unwrap();
    let node = nodes.items[0].id.clone();
    let f = s.scheduler_form("settings", Some(&node)).await.unwrap();
    let mut v = values_of(&f);
    set(&mut v, KEY, json!(25));
    save(&mut s, "settings", Some(&node), &v).await;
    let (_, val) = h
        .send(
            Method::GET,
            &format!("/_node/_local/_config/replicator/{KEY}"),
            None,
        )
        .await;
    assert_eq!(val, "25");
    let f = s.scheduler_form("settings", Some(&node)).await.unwrap();
    assert_eq!(f.values[KEY], 25);
    let mut v = values_of(&f);
    set(&mut v, KEY, Value::Null);
    let script = s
        .scheduler_script("settings", Some(&node), &v)
        .await
        .unwrap();
    assert!(
        script.contains("DELETE /_node/") && script.contains(KEY),
        "{script}"
    );
    save(&mut s, "settings", Some(&node), &v).await;
    let (st, _) = h
        .send(
            Method::GET,
            &format!("/_node/_local/_config/replicator/{KEY}"),
            None,
        )
        .await;
    assert_eq!(st, 404, "back to the default");

    // Ids that aren't the server's are refused before anything is sent.
    for (kind, bad) in [
        ("job", "doc:users:x"),
        ("job", "doc:_replicator:_design/x"),
        ("job", "doc:_replicator:.."),
        ("job", "rep:xyz"),
        ("job", "dbine_test_sched_a"),
        ("job", "doc:_replicator:dbine_test_sched_nope"),
        ("settings", "other@nowhere"),
        ("nope", "x"),
    ] {
        assert!(
            s.scheduler_form(kind, Some(bad)).await.is_err(),
            "{kind} {bad:?}"
        );
        assert!(
            s.scheduler_delete_script(kind, bad).await.is_err(),
            "{kind} {bad:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn replications_managed_whole() {
    let Some((c, h)) = cfg() else {
        eprintln!("DBINE_TEST_COUCHDB_URL not set; skipping");
        return;
    };
    let (st, _) = h.send(reqwest::Method::GET, "/_replicator", None).await;
    let made_replicator = st == 404;
    let (st, v) = h
        .send(
            reqwest::Method::GET,
            &format!("/_node/_local/_config/replicator/{KEY}"),
            None,
        )
        .await;
    let setting = (st == 200).then(|| v.as_str().unwrap_or_default().to_string());
    let left = cleanup(&h, false, setting.clone()).await;
    assert!(left.is_empty(), "leftovers: {left:?}");
    let r = tokio::spawn(body(c, h.clone())).await;
    let left = cleanup(&h, made_replicator, setting).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
    assert!(left.is_empty(), "left behind: {left:?}");
}

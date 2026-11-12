//! Server jobs against real clusters (each test needs its variable):
//! - `DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520`: Elasticsearch
//!   8.15 with a basic license: transforms, and a note for Watcher.
//! - `DBINE_TEST_ELASTICSEARCH_TRIAL_URL=http://elastic:dbine-test-pass@localhost:25530`:
//!   Elasticsearch 8.15 with `xpack.license.self_generated.type=trial` and
//!   security on: watches and their history, and what a user without the
//!   privileges sees.
//!
//!   ```sh
//!   docker run -d --name dbine-test-jobs-elasticsearch -p 25530:9200 \
//!     -e discovery.type=single-node -e xpack.security.enabled=true \
//!     -e xpack.security.http.ssl.enabled=false \
//!     -e xpack.security.transport.ssl.enabled=false \
//!     -e ELASTIC_PASSWORD=dbine-test-pass \
//!     -e xpack.license.self_generated.type=trial \
//!     docker.elastic.co/elasticsearch/elasticsearch:8.15.3
//!   ```
//! - `DBINE_TEST_OPENSEARCH_URL=http://localhost:25521`: OpenSearch 2.x
//!   transforms and alerting monitors.
//!
//! Run with `cargo test -p dbine-driver-elasticsearch --test jobs -- --ignored`.
//! Each test makes its own `dbine_test_jobs_*` objects and removes them at
//! the end, also when an assertion fails.

use dbine_driver::{ConnectionConfig, JobOutcome, ServerJob, Session};
use serde_json::json;
use std::time::Duration;

/// Connects; a `user:password@` in the URL becomes the login.
async fn open(driver: &str, url: &str) -> Box<dyn Session> {
    let d = dbine_driver_elasticsearch::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    assert!(d.capabilities().jobs);
    let (host, login) = split_login(url);
    let cfg = ConnectionConfig {
        driver: driver.into(),
        host,
        username: login.as_ref().map(|l| l.0.clone()),
        password: login.as_ref().map(|l| l.1.clone()),
        ..Default::default()
    };
    d.connect(&cfg, None).await.unwrap()
}

/// `http://u:p@host` → (`http://host`, (u, p)).
fn split_login(url: &str) -> (String, Option<(String, String)>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.into(), None);
    };
    match rest.split_once('@') {
        Some((login, host)) => {
            let (u, p) = login.split_once(':').unwrap_or((login, ""));
            (format!("{scheme}://{host}"), Some((u.into(), p.into())))
        }
        None => (url.into(), None),
    }
}

async fn find(s: &mut Box<dyn Session>, name: &str) -> Option<ServerJob> {
    s.jobs()
        .await
        .unwrap()
        .jobs
        .into_iter()
        .find(|j| j.name == name)
}

async fn wait_for(
    s: &mut Box<dyn Session>,
    name: &str,
    f: impl Fn(&ServerJob) -> bool,
) -> ServerJob {
    for _ in 0..120 {
        if let Some(j) = find(s, name).await.filter(|j| f(j)) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{name}: {:?}", find(s, name).await);
}

async fn wait_for_gone(s: &mut Box<dyn Session>, name: &str) {
    for _ in 0..120 {
        if find(s, name).await.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{name} is still listed");
}

async fn http(
    method: reqwest::Method,
    url: &str,
    body: Option<serde_json::Value>,
) -> serde_json::Value {
    let (url, login) = split_login(url);
    let mut rb = reqwest::Client::new().request(method, url);
    if let Some((u, p)) = login {
        rb = rb.basic_auth(u, Some(p));
    }
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    let r = rb.send().await.unwrap();
    r.json().await.unwrap_or(serde_json::Value::Null)
}

async fn refuses(s: &mut Box<dyn Session>, bad: &[&str]) {
    for id in bad {
        assert!(s.job_detail(id).await.is_err(), "{id}");
        assert!(s.run_job(id).await.is_err(), "{id}");
        assert!(s.set_job_enabled(id, false).await.is_err(), "{id}");
        assert!(s.job_script(id).await.is_err(), "{id}");
    }
}

async fn source_index(url: &str) {
    http(
        reqwest::Method::PUT,
        &format!("{url}/dbine_test_jobs_src"),
        Some(json!({"mappings": {"properties": {"host": {"type": "keyword"}, "v": {"type": "double"}, "t": {"type": "date"}}}})),
    )
    .await;
    let r = reqwest::Client::new()
        .post(format!("{url}/dbine_test_jobs_src/_bulk?refresh=true"))
        .header("Content-Type", "application/x-ndjson")
        .body("{\"index\":{}}\n{\"host\":\"a\",\"v\":1,\"t\":\"2026-11-12T10:00:00Z\"}\n{\"index\":{}}\n{\"host\":\"b\",\"v\":2,\"t\":\"2026-11-12T10:01:00Z\"}\n")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
}

// ------------------------------------------------- Elasticsearch, basic

async fn es_clean(url: &str) {
    for id in [
        "dbine_test_jobs_tf",
        "dbine_test_jobs_batch",
        "dbine_test_jobs_failing",
    ] {
        http(
            reqwest::Method::DELETE,
            &format!("{url}/_transform/{id}?force=true"),
            None,
        )
        .await;
    }
    // By name: Elasticsearch 8 refuses wildcard deletes.
    http(
        reqwest::Method::DELETE,
        &format!(
            "{url}/dbine_test_jobs_src,dbine_test_jobs_dst,dbine_test_jobs_dst2,dbine_test_jobs_dst3?ignore_unavailable=true"
        ),
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn elasticsearch_transforms() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") else {
        eprintln!("DBINE_TEST_ELASTICSEARCH_URL not set; skipping");
        return;
    };
    es_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move { es_body(&u).await }).await;
    es_clean(&url).await;
    body.unwrap();
}

async fn es_body(url: &str) {
    source_index(url).await;
    let pivot = json!({"group_by": {"host": {"terms": {"field": "host"}}}, "aggregations": {"avg_v": {"avg": {"field": "v"}}}});
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_transform/dbine_test_jobs_tf"),
        Some(json!({"source": {"index": "dbine_test_jobs_src"}, "dest": {"index": "dbine_test_jobs_dst"}, "frequency": "1m", "sync": {"time": {"field": "t", "delay": "60s"}}, "pivot": pivot, "description": "dbine test"})),
    )
    .await;
    assert_eq!(r["acknowledged"], true, "{r}");
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_transform/dbine_test_jobs_batch"),
        Some(json!({"source": {"index": "dbine_test_jobs_src"}, "dest": {"index": "dbine_test_jobs_dst2"}, "pivot": pivot})),
    )
    .await;
    assert_eq!(r["acknowledged"], true, "{r}");

    let mut s = open("elasticsearch", url).await;
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    assert!(
        list.notes.iter().any(|n| n.contains("licencia")),
        "basic: Watcher isn't there"
    );
    let tf = find(&mut s, "dbine_test_jobs_tf").await.unwrap();
    assert_eq!(tf.id, "transform:dbine_test_jobs_tf");
    assert_eq!(tf.kind, "Transform");
    assert_eq!(tf.schedule.as_deref(), Some("frequency 1m"));
    assert!(!tf.enabled && !tf.can_run && tf.can_enable && tf.has_script && !tf.has_history);
    let batch = find(&mut s, "dbine_test_jobs_batch").await.unwrap();
    assert!(batch.schedule.is_none());

    let d = s.job_detail(&tf.id).await.unwrap();
    assert!(d
        .properties
        .iter()
        .any(|p| p.label == "Destino" && p.value == "dbine_test_jobs_dst"));
    assert_eq!(d.steps[0].kind.as_deref(), Some("pivot"));

    // Enable (start): its first checkpoint runs.
    s.set_job_enabled(&tf.id, true).await.unwrap();
    let on = wait_for(&mut s, "dbine_test_jobs_tf", |j| {
        j.enabled && j.last_outcome == Some(JobOutcome::Succeeded)
    })
    .await;
    assert!(on.can_run && on.last_started.as_deref().is_some_and(|t| t.ends_with('Z')));
    // Ids that name several transforms, or a path the URL would rewrite,
    // are refused, and the transform keeps running.
    refuses(
        &mut s,
        &[
            "transform:_all",
            "transform:*",
            "transform:dbine_test_jobs_*",
            "transform:dbine_test_jobs_tf,dbine_test_jobs_batch",
            "transform:.",
            "transform:..",
            "transform:DBINE_TEST_JOBS_TF",
        ],
    )
    .await;
    assert!(find(&mut s, "dbine_test_jobs_tf").await.unwrap().enabled);
    // Run now: one more check.
    s.run_job(&tf.id).await.unwrap();
    assert!(s.job_runs(&tf.id, 5).await.is_err(), "no history");
    s.set_job_enabled(&tf.id, false).await.unwrap();
    let off = wait_for(&mut s, "dbine_test_jobs_tf", |j| !j.enabled).await;
    assert!(!off.can_run);
    assert!(s.run_job(&tf.id).await.is_err());

    // The script recreates it.
    let script = s.job_script(&tf.id).await.unwrap();
    eprintln!("{script}");
    assert!(script.starts_with("PUT /_transform/dbine_test_jobs_tf\n"));
    let (line, body) = script.split_once('\n').unwrap();
    assert!(line.ends_with("dbine_test_jobs_tf"));
    http(
        reqwest::Method::DELETE,
        &format!("{url}/_transform/dbine_test_jobs_tf?force=true"),
        None,
    )
    .await;
    assert!(find(&mut s, "dbine_test_jobs_tf").await.is_none());
    let body: serde_json::Value = serde_json::from_str(body).unwrap();
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_transform/dbine_test_jobs_tf"),
        Some(body),
    )
    .await;
    assert_eq!(r["acknowledged"], true, "{r}");
    assert!(find(&mut s, "dbine_test_jobs_tf").await.is_some());

    refuses(
        &mut s,
        &[
            "x' OR 1=1 --",
            "dbine_test_jobs_tf",
            "transform:",
            "watch:x",
            "monitor:x",
            "transform:dbine_test_jobs_tf\n",
        ],
    )
    .await;
    // A path in the id stays one segment.
    let e = s
        .job_detail("transform:../_cluster/settings")
        .await
        .unwrap_err();
    eprintln!("{e}");
    assert!(s
        .set_job_enabled("transform:dbine_test_jobs_tf/_stop?x=", true)
        .await
        .is_err());

    // A failed transform is stopped (forced) when disabled. Its destination
    // refuses writes and it doesn't retry, so it fails on its first run.
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/dbine_test_jobs_dst3"),
        Some(json!({"settings": {"index.blocks.write": true}})),
    )
    .await;
    assert_eq!(r["acknowledged"], true, "{r}");
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_transform/dbine_test_jobs_failing?defer_validation=true"),
        Some(json!({"source": {"index": "dbine_test_jobs_src"}, "dest": {"index": "dbine_test_jobs_dst3"}, "frequency": "1s", "sync": {"time": {"field": "t", "delay": "1s"}}, "pivot": pivot, "settings": {"num_failure_retries": 0}})),
    )
    .await;
    assert_eq!(r["acknowledged"], true, "{r}");
    s.set_job_enabled("transform:dbine_test_jobs_failing", true)
        .await
        .unwrap();
    let failing = wait_for(&mut s, "dbine_test_jobs_failing", |j| {
        j.last_outcome == Some(JobOutcome::Failed)
    })
    .await;
    assert!(failing.enabled && failing.can_enable);
    s.set_job_enabled(&failing.id, false).await.unwrap();
    wait_for(&mut s, "dbine_test_jobs_failing", |j| !j.enabled).await;
}

// ------------------------------------------------- Elasticsearch, trial

async fn watcher_clean(url: &str) {
    for id in ["dbine_test_jobs_w", "dbine_test_jobs_fail"] {
        http(
            reqwest::Method::DELETE,
            &format!("{url}/_watcher/watch/{id}"),
            None,
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn elasticsearch_watches() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_TRIAL_URL") else {
        eprintln!("DBINE_TEST_ELASTICSEARCH_TRIAL_URL not set; skipping");
        return;
    };
    watcher_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move { watcher_body(&u).await }).await;
    watcher_clean(&url).await;
    body.unwrap();
}

async fn watcher_body(url: &str) {
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_watcher/watch/dbine_test_jobs_w"),
        Some(json!({"trigger": {"schedule": {"interval": "1h"}}, "input": {"simple": {"x": 1}}, "condition": {"always": {}}, "actions": {"log1": {"logging": {"text": "dbine {{ctx.watch_id}}"}}}, "metadata": {"name": "dbine test"}})),
    )
    .await;
    assert_eq!(r["created"], true, "{r}");
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_watcher/watch/dbine_test_jobs_fail"),
        Some(json!({"trigger": {"schedule": {"cron": "0 0 3 * * ?"}}, "input": {"simple": {}}, "condition": {"always": {}}, "actions": {"hook": {"webhook": {"method": "POST", "host": "127.0.0.1", "port": 9, "path": "/x", "body": "x"}}}})),
    )
    .await;
    assert_eq!(r["created"], true, "{r}");

    let mut s = open("elasticsearch", url).await;
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    let w = find(&mut s, "dbine_test_jobs_w").await.unwrap();
    assert_eq!(w.id, "watch:dbine_test_jobs_w");
    assert_eq!(w.kind, "Watch");
    assert_eq!(w.description.as_deref(), Some("dbine test"));
    assert_eq!(w.schedule.as_deref(), Some("interval 1h"));
    assert!(w.enabled && w.can_run && w.can_enable && w.has_history && w.has_script);
    let fail = find(&mut s, "dbine_test_jobs_fail").await.unwrap();
    assert_eq!(fail.schedule.as_deref(), Some("cron 0 0 3 * * ?"));

    let d = s.job_detail(&w.id).await.unwrap();
    assert_eq!(d.steps[0].name, "log1");
    assert_eq!(d.steps[0].kind.as_deref(), Some("logging"));

    // Run now; the run is recorded with its action.
    s.run_job(&w.id).await.unwrap();
    s.run_job(&fail.id).await.unwrap();
    let mut runs = Vec::new();
    // The first recorded run creates the history data stream: until its
    // shard is up, the search can fail.
    for _ in 0..60 {
        runs = s.job_runs(&w.id, 10).await.unwrap_or_default();
        if !runs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("{runs:#?}");
    assert_eq!(runs[0].outcome, JobOutcome::Succeeded);
    assert_eq!(runs[0].trigger.as_deref(), Some("manual"));
    assert_eq!(runs[0].steps[0].name, "log1");
    assert_eq!(runs[0].steps[0].outcome, JobOutcome::Succeeded);
    let ran = wait_for(&mut s, "dbine_test_jobs_w", |j| {
        j.last_outcome == Some(JobOutcome::Succeeded)
    })
    .await;
    assert!(ran
        .last_started
        .as_deref()
        .is_some_and(|t| t.ends_with('Z')));
    let mut failed = Vec::new();
    for _ in 0..60 {
        failed = s.job_runs(&fail.id, 10).await.unwrap_or_default();
        if !failed.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("{failed:#?}");
    assert_eq!(failed[0].outcome, JobOutcome::Failed);
    assert_eq!(failed[0].steps[0].outcome, JobOutcome::Failed);

    // Disable and enable.
    s.set_job_enabled(&w.id, false).await.unwrap();
    assert!(!find(&mut s, "dbine_test_jobs_w").await.unwrap().enabled);
    let script = s.job_script(&w.id).await.unwrap();
    assert!(
        script.starts_with("PUT /_watcher/watch/dbine_test_jobs_w?active=false\n"),
        "{script}"
    );
    s.set_job_enabled(&w.id, true).await.unwrap();
    assert!(find(&mut s, "dbine_test_jobs_w").await.unwrap().enabled);
    assert!(s.stop_job(&w.id).await.is_err());

    // The script recreates it.
    let script = s.job_script(&w.id).await.unwrap();
    let (_, body) = script.split_once('\n').unwrap();
    http(
        reqwest::Method::DELETE,
        &format!("{url}/_watcher/watch/dbine_test_jobs_w"),
        None,
    )
    .await;
    assert!(find(&mut s, "dbine_test_jobs_w").await.is_none());
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_watcher/watch/dbine_test_jobs_w"),
        Some(serde_json::from_str(body).unwrap()),
    )
    .await;
    assert_eq!(r["created"], true, "{r}");

    refuses(
        &mut s,
        &[
            "x' OR 1=1 --",
            "monitor:x",
            "dbine_test_jobs_w",
            "watch:..",
            "watch:.",
            "watch:_all",
            "watch:dbine_test_jobs_*",
            "watch:dbine_test_jobs_w,dbine_test_jobs_fail",
        ],
    )
    .await;
    assert!(find(&mut s, "dbine_test_jobs_w").await.unwrap().enabled);
    let e = s
        .job_detail("watch:../../_cluster/settings")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no existe"), "{e}");
}

// ------------------------------- Elasticsearch, a user without privileges

const LIMITED_USERS: [&str; 1] = ["dbine_test_jobs_monitor"];

async fn limited_clean(url: &str) {
    for u in LIMITED_USERS {
        http(
            reqwest::Method::DELETE,
            &format!("{url}/_security/user/{u}"),
            None,
        )
        .await;
        http(
            reqwest::Method::DELETE,
            &format!("{url}/_security/role/{u}"),
            None,
        )
        .await;
    }
    http(
        reqwest::Method::DELETE,
        &format!("{url}/_watcher/watch/dbine_test_jobs_seen"),
        None,
    )
    .await;
}

/// The trial cluster has security on: a user who sees the watches but may
/// not read their history gets them without history, and a note. (A user
/// who may not list transforms or watches can't connect either: DBine's
/// `GET /` needs `monitor`, which includes both; the unit tests cover how
/// those refusals are read.)
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn elasticsearch_limited_user() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_TRIAL_URL") else {
        eprintln!("DBINE_TEST_ELASTICSEARCH_TRIAL_URL not set; skipping");
        return;
    };
    if !url.contains('@') {
        eprintln!("DBINE_TEST_ELASTICSEARCH_TRIAL_URL has no login (security off); skipping");
        return;
    }
    limited_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move { limited_body(&u).await }).await;
    limited_clean(&url).await;
    body.unwrap();
}

async fn limited_body(url: &str) {
    let (_, host) = url.split_once('@').unwrap();
    let scheme = url.split_once("://").unwrap().0;
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_watcher/watch/dbine_test_jobs_seen"),
        Some(json!({"trigger": {"schedule": {"interval": "1h"}}, "input": {"simple": {}}, "condition": {"always": {}}, "actions": {"log1": {"logging": {"text": "x"}}}})),
    )
    .await;
    assert_eq!(r["created"], true, "{r}");
    let user = LIMITED_USERS[0];
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_security/role/{user}"),
        Some(json!({"cluster": ["monitor"]})),
    )
    .await;
    assert_eq!(r["role"]["created"], true, "{r}");
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_security/user/{user}"),
        Some(json!({"password": "dbine-test-pass", "roles": [user]})),
    )
    .await;
    assert_eq!(r["created"], true, "{r}");

    // The watches, but not their history.
    let mut s = open(
        "elasticsearch",
        &format!("{scheme}://{user}:dbine-test-pass@{host}"),
    )
    .await;
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    let w = list
        .jobs
        .iter()
        .find(|j| j.name == "dbine_test_jobs_seen")
        .unwrap();
    assert!(!w.has_history);
    assert!(list.notes.iter().any(|n| n.contains(".watcher-history")));
    assert!(!s.job_detail(&w.id).await.unwrap().job.has_history);
    let e = s.job_runs(&w.id, 5).await.unwrap_err();
    assert!(e.to_string().contains(".watcher-history"), "{e}");

    // The superuser sees the history.
    let mut s = open("elasticsearch", url).await;
    let w = find(&mut s, "dbine_test_jobs_seen").await.unwrap();
    assert!(w.has_history);
}

// ------------------------------------------------------------ OpenSearch

async fn monitors_clean(url: &str, prefix: &str) {
    let found = http(
        reqwest::Method::POST,
        &format!("{url}/{prefix}/_alerting/monitors/_search"),
        Some(json!({"size": 100, "query": {"match_all": {}}})),
    )
    .await;
    for h in found["hits"]["hits"].as_array().into_iter().flatten() {
        if h["_source"]["name"]
            .as_str()
            .is_some_and(|n| n.starts_with("dbine_test_jobs_"))
        {
            http(
                reqwest::Method::DELETE,
                &format!(
                    "{url}/{prefix}/_alerting/monitors/{}",
                    h["_id"].as_str().unwrap()
                ),
                None,
            )
            .await;
        }
    }
}

/// Open Distro: monitors under `_opendistro`, and a note for its
/// transforms (no list API).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opendistro_monitors() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENDISTRO_URL") else {
        eprintln!("DBINE_TEST_OPENDISTRO_URL not set; skipping");
        return;
    };
    monitors_clean(&url, "_opendistro").await;
    let u = url.clone();
    let body = tokio::spawn(async move {
        let url = u.as_str();
        let mut s = open("opendistro", url).await;
        let list = s.jobs().await.unwrap();
        eprintln!("{list:#?}");
        assert!(list.notes.iter().any(|n| n.contains("liste los transforms")));
        let r = http(
            reqwest::Method::POST,
            &format!("{url}/_opendistro/_alerting/monitors"),
            Some(json!({"type": "monitor", "name": "dbine_test_jobs_mon", "enabled": true, "schedule": {"period": {"interval": 1, "unit": "HOURS"}}, "inputs": [{"search": {"indices": ["*"], "query": {"size": 0, "query": {"match_all": {}}}}}], "triggers": [{"name": "t1", "severity": "1", "condition": {"script": {"source": "false", "lang": "painless"}}, "actions": []}]})),
        )
        .await;
        assert!(r["_id"].is_string(), "{r}");
        let mon = wait_for(&mut s, "dbine_test_jobs_mon", |_| true).await;
        assert!(mon.enabled && mon.can_run && mon.can_enable && mon.has_script);
        s.run_job(&mon.id).await.unwrap();
        s.set_job_enabled(&mon.id, false).await.unwrap();
        assert!(!s.job_detail(&mon.id).await.unwrap().job.enabled);
        s.set_job_enabled(&mon.id, true).await.unwrap();
        assert!(s.job_detail(&mon.id).await.unwrap().job.enabled);
        let script = s.job_script(&mon.id).await.unwrap();
        assert!(script.starts_with("POST /_opendistro/_alerting/monitors\n"), "{script}");
        refuses(
            &mut s,
            &["x' OR 1=1 --", "watch:x", "monitor:_all", "monitor:..", "monitor:_search"],
        )
        .await;
    })
    .await;
    monitors_clean(&url, "_opendistro").await;
    body.unwrap();
}

async fn os_clean(url: &str) {
    http(
        reqwest::Method::POST,
        &format!("{url}/_plugins/_transform/dbine_test_jobs_tf/_stop"),
        None,
    )
    .await;
    http(
        reqwest::Method::DELETE,
        &format!("{url}/_plugins/_transform/dbine_test_jobs_tf?force=true"),
        None,
    )
    .await;
    let found = http(
        reqwest::Method::POST,
        &format!("{url}/_plugins/_alerting/monitors/_search"),
        Some(json!({"size": 100, "query": {"match_all": {}}})),
    )
    .await;
    for h in found["hits"]["hits"].as_array().into_iter().flatten() {
        if h["_source"]["name"]
            .as_str()
            .is_some_and(|n| n.starts_with("dbine_test_jobs_"))
        {
            http(
                reqwest::Method::DELETE,
                &format!(
                    "{url}/_plugins/_alerting/monitors/{}",
                    h["_id"].as_str().unwrap()
                ),
                None,
            )
            .await;
        }
    }
    // Written straight into the config index by the test.
    http(
        reqwest::Method::DELETE,
        &format!("{url}/.opendistro-alerting-config/_doc/dbine_test_jobs_owned?refresh=true"),
        None,
    )
    .await;
    http(
        reqwest::Method::DELETE,
        &format!("{url}/dbine_test_jobs_*?ignore_unavailable=true"),
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opensearch_transforms_and_monitors() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else {
        eprintln!("DBINE_TEST_OPENSEARCH_URL not set; skipping");
        return;
    };
    os_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move { os_body(&u).await }).await;
    os_clean(&url).await;
    body.unwrap();
}

async fn os_body(url: &str) {
    source_index(url).await;
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/_plugins/_transform/dbine_test_jobs_tf"),
        Some(json!({"transform": {"enabled": false, "continuous": false, "schedule": {"interval": {"period": 1, "unit": "Minutes", "start_time": 1602100553}}, "description": "dbine test", "source_index": "dbine_test_jobs_src", "target_index": "dbine_test_jobs_dst", "data_selection_query": {"match_all": {}}, "page_size": 10, "groups": [{"terms": {"source_field": "host", "target_field": "host"}}], "aggregations": {"avg_v": {"avg": {"field": "v"}}}}})),
    )
    .await;
    assert_eq!(r["_id"], "dbine_test_jobs_tf", "{r}");
    let r = http(
        reqwest::Method::POST,
        &format!("{url}/_plugins/_alerting/monitors"),
        Some(json!({"type": "monitor", "monitor_type": "query_level_monitor", "name": "dbine_test_jobs_mon", "enabled": true, "schedule": {"period": {"interval": 1, "unit": "HOURS"}}, "inputs": [{"search": {"indices": ["dbine_test_jobs_src"], "query": {"size": 0, "query": {"match_all": {}}}}}], "triggers": [{"name": "t1", "severity": "1", "condition": {"script": {"source": "ctx.results[0].hits.total.value > 0", "lang": "painless"}}, "actions": []}]})),
    )
    .await;
    assert!(r["_id"].is_string(), "{r}");

    let mut s = open("opensearch", url).await;
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    let tf = find(&mut s, "dbine_test_jobs_tf").await.unwrap();
    assert_eq!(tf.id, "transform:dbine_test_jobs_tf");
    assert_eq!(tf.schedule.as_deref(), Some("every 1 Minutes"));
    assert!(!tf.enabled && tf.can_enable && !tf.can_run && tf.has_script);
    let mon = find(&mut s, "dbine_test_jobs_mon").await.unwrap();
    assert_eq!(mon.kind, "Monitor");
    assert_eq!(mon.schedule.as_deref(), Some("every 1 HOURS"));
    assert!(mon.enabled && mon.can_run && mon.can_enable && mon.has_script);

    let d = s.job_detail(&mon.id).await.unwrap();
    assert!(d.steps.iter().any(|st| st.name == "t1"));
    let d = s.job_detail(&tf.id).await.unwrap();
    assert!(d
        .properties
        .iter()
        .any(|p| p.label == "Origen" && p.value == "dbine_test_jobs_src"));

    // The transform: enable runs it on its schedule; disable stops it. The
    // list is a search: it sees the change after the index refreshes.
    s.set_job_enabled(&tf.id, true).await.unwrap();
    assert!(s.job_detail(&tf.id).await.unwrap().job.enabled);
    wait_for(&mut s, "dbine_test_jobs_tf", |j| j.enabled).await;
    s.set_job_enabled(&tf.id, false).await.unwrap();
    assert!(!s.job_detail(&tf.id).await.unwrap().job.enabled);
    wait_for(&mut s, "dbine_test_jobs_tf", |j| !j.enabled).await;
    assert!(s.run_job(&tf.id).await.is_err());

    // The monitor: run now, disable and enable; the update keeps its owner
    // and data sources.
    let mon_id = mon.id.strip_prefix("monitor:").unwrap().to_string();
    let monitor_url = format!("{url}/_plugins/_alerting/monitors/{mon_id}");
    let before = http(reqwest::Method::GET, &monitor_url, None).await;
    s.run_job(&mon.id).await.unwrap();
    s.set_job_enabled(&mon.id, false).await.unwrap();
    assert!(!s.job_detail(&mon.id).await.unwrap().job.enabled);
    wait_for(&mut s, "dbine_test_jobs_mon", |j| !j.enabled).await;
    s.set_job_enabled(&mon.id, true).await.unwrap();
    wait_for(&mut s, "dbine_test_jobs_mon", |j| j.enabled).await;
    let after = http(reqwest::Method::GET, &monitor_url, None).await;
    for k in ["owner", "data_sources", "inputs", "triggers", "schedule"] {
        assert_eq!(before["monitor"][k], after["monitor"][k], "{k}");
    }
    assert!(s.job_runs(&mon.id, 5).await.is_err(), "no history");

    // A monitor another plugin made (Security Analytics' detectors give
    // theirs their own owner and indices; it's written straight into the
    // config index here, which the REST API won't do). Alerting's search
    // leaves it out of the list; by its id, the REST update would refuse
    // it, so enabling isn't offered and it stays as it was.
    let owned_doc = json!({"monitor": {"type": "monitor", "schema_version": 8, "name": "dbine_test_jobs_owned", "monitor_type": "doc_level_monitor", "enabled": true, "enabled_time": 1794489425309u64, "owner": "security_analytics", "data_sources": {"alerts_index": ".opensearch-sap-dbine-test-jobs-alerts", "alerts_history_index": ".opensearch-sap-dbine-test-jobs-alerts-history", "alerts_history_index_pattern": "<.opensearch-sap-dbine-test-jobs-alerts-history-{now/d}-1>", "findings_index": ".opensearch-sap-dbine-test-jobs-findings", "findings_index_pattern": "<.opensearch-sap-dbine-test-jobs-findings-{now/d}-1>", "query_index": ".opensearch-sap-dbine-test-jobs-queries", "findings_enabled": true}, "schedule": {"period": {"interval": 1, "unit": "HOURS"}}, "inputs": [{"doc_level_input": {"description": "", "indices": ["dbine_test_jobs_src"], "queries": [{"id": "q1", "name": "q1", "query": "host:\"a\"", "tags": []}]}}], "triggers": [], "last_update_time": 1794489425309u64}});
    let r = http(
        reqwest::Method::PUT,
        &format!("{url}/.opendistro-alerting-config/_doc/dbine_test_jobs_owned?refresh=true"),
        Some(owned_doc),
    )
    .await;
    assert_eq!(r["result"], "created", "{r}");
    assert!(find(&mut s, "dbine_test_jobs_owned").await.is_none());
    let d = s.job_detail("monitor:dbine_test_jobs_owned").await.unwrap();
    let owned = d.job.clone();
    assert!(owned.enabled && !owned.can_enable && owned.can_run);
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("lo administra security_analytics")),
        "{:?}",
        d.notes
    );
    let e = s.set_job_enabled(&owned.id, false).await.unwrap_err();
    assert!(e.to_string().contains("security_analytics"), "{e}");
    let still = http(
        reqwest::Method::GET,
        &format!("{url}/_plugins/_alerting/monitors/dbine_test_jobs_owned"),
        None,
    )
    .await;
    assert_eq!(still["monitor"]["owner"], "security_analytics", "{still}");
    assert_eq!(still["monitor"]["enabled"], true, "{still}");
    assert_eq!(
        still["monitor"]["data_sources"]["alerts_index"],
        ".opensearch-sap-dbine-test-jobs-alerts"
    );

    // The scripts recreate them.
    let script = s.job_script(&mon.id).await.unwrap();
    eprintln!("{script}");
    let (line, body) = script.split_once('\n').unwrap();
    assert_eq!(line, "POST /_plugins/_alerting/monitors");
    http(
        reqwest::Method::DELETE,
        &format!("{url}/_plugins/_alerting/monitors/{mon_id}"),
        None,
    )
    .await;
    wait_for_gone(&mut s, "dbine_test_jobs_mon").await;
    let r = http(
        reqwest::Method::POST,
        &format!("{url}/_plugins/_alerting/monitors"),
        Some(serde_json::from_str(body).unwrap()),
    )
    .await;
    assert!(r["_id"].is_string(), "{r}");
    wait_for(&mut s, "dbine_test_jobs_mon", |_| true).await;

    let script = s.job_script(&tf.id).await.unwrap();
    eprintln!("{script}");
    assert!(script.starts_with("PUT /_plugins/_transform/dbine_test_jobs_tf\n"));

    refuses(
        &mut s,
        &[
            "x' OR 1=1 --",
            "watch:x",
            "dbine_test_jobs_tf",
            "monitor:",
            "monitor:_all",
            "monitor:..",
            "monitor:_search",
            "monitor:alerts",
            "transform:_all",
            "transform:*",
            "transform:dbine_test_jobs_tf,dbine_test_jobs_x",
            "transform:.",
        ],
    )
    .await;
    assert!(s
        .job_detail("monitor:../../dbine_test_jobs_src/_doc/1")
        .await
        .is_err());
}

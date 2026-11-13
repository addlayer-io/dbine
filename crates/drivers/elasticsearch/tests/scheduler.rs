//! The scheduler designer against real clusters (each test needs its
//! variable, and skips without it):
//!
//! - `DBINE_TEST_ES_SCHED_URL=http://localhost:25533`: Elasticsearch 8.15
//!   with a trial license (Watcher) and `path.repo` (SLM):
//!
//!   ```sh
//!   docker run -d --name dbine-test-sched-es -p 25533:9200 -e discovery.type=single-node \
//!     -e xpack.security.enabled=false -e "ES_JAVA_OPTS=-Xms512m -Xmx512m" \
//!     -e path.repo=/tmp/snaps -e xpack.license.self_generated.type=trial \
//!     docker.elastic.co/elasticsearch/elasticsearch:8.15.3
//!   ```
//! - `DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520`
//!   (`dbine-test-elasticsearch`, basic license): transforms, and Watcher
//!   left out with a note.
//! - `DBINE_TEST_OPENSEARCH_URL=http://localhost:25521`
//!   (`dbine-test-opensearch`, 2.17): transforms, monitors, snapshot
//!   management policies.
//! - `DBINE_TEST_OPENDISTRO_URL=http://localhost:25524`
//!   (`dbine-test-opendistro`, 1.13): monitors.
//!
//! ```sh
//! cargo test -p dbine-driver-elasticsearch --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! Every object a test makes is named `dbine_test_sched_*` and is removed
//! at the end, whether the test passed or not.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, FormValues, ObjectForm, Session, SECRET_MASK};
use serde_json::{json, Value};
use std::future::Future;
use std::time::Duration;

const PREFIX: &str = "dbine_test_sched_";
/// A watch made as Monitoring makes its own.
const SYS_WATCH: &str = "dbine_test_sched_wsys";
/// Elastic Cloud's snapshot policy, made by the snapshot test with
/// `config.metadata.dbine_test_sched` so the cleanup knows it's its own.
const CLOUD_SLM: &str = "cloud-snapshot-policy";

fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(u) if !u.is_empty() => Some(u.trim_end_matches('/').to_string()),
        _ => {
            eprintln!("{var} not set; skipping");
            None
        }
    }
}

async fn open(driver: &str, url: &str) -> Box<dyn Session> {
    let d = dbine_driver_elasticsearch::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    assert!(d.capabilities().scheduler);
    let cfg = ConnectionConfig {
        driver: driver.into(),
        host: url.into(),
        ..Default::default()
    };
    d.connect(&cfg, None).await.unwrap()
}

/// A request to the cluster outside the driver: (status, body).
async fn http(method: &str, url: &str, body: Option<Value>) -> (u16, Value) {
    let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let mut rb = reqwest::Client::new().request(m, url);
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    let r = rb.send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// A call that must be refused: its message.
fn refusal<T: std::fmt::Debug>(r: dbine_driver::Result<T>) -> String {
    match r {
        Err(e) => e.to_string(),
        Ok(x) => panic!("expected a refusal, got {x:?}"),
    }
}

/// Runs the test, then the cleanup, and only then lets a failed assertion
/// through.
async fn with_cleanup<F, C, CF>(body: F, cleanup: C)
where
    F: Future<Output = ()> + Send + 'static,
    C: FnOnce() -> CF,
    CF: Future<Output = usize>,
{
    let r = tokio::spawn(body).await;
    let left = cleanup().await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
    assert_eq!(left, 0, "objects left behind");
}

/// What the designer would send back for a form as it came: its values and
/// the fields' defaults where a value is missing.
fn values_of(form: &ObjectForm) -> FormValues {
    let mut values = form.values.clone();
    for f in form.sections.iter().flat_map(|s| s.fields.iter()) {
        if let Some(d) = &f.default {
            values.entry(f.key.clone()).or_insert_with(|| d.clone());
        }
    }
    let lists = form
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .map(|l| (l.key.clone(), l.rows.clone()))
        .collect();
    FormValues { values, lists }
}

fn set(v: &mut FormValues, pairs: Value) {
    for (k, x) in pairs.as_object().unwrap() {
        v.values.insert(k.clone(), x.clone());
    }
}

/// A new job's form with `values` over its defaults.
async fn new_job(s: &mut Box<dyn Session>, values: Value) -> FormValues {
    let form = s.scheduler_form("job", None).await.unwrap();
    assert!(!form.read_only, "{:?}", form.notes);
    let mut v = values_of(&form);
    set(&mut v, values);
    v
}

async fn save(s: &mut Box<dyn Session>, id: Option<&str>, v: &FormValues) -> String {
    match s.scheduler_save("job", id, v).await {
        Ok(Some(id)) => id,
        other => panic!("save {id:?}: {other:?}\n{}", script(s, id, v).await),
    }
}

async fn script(s: &mut Box<dyn Session>, id: Option<&str>, v: &FormValues) -> String {
    s.scheduler_script("job", id, v)
        .await
        .unwrap_or_else(|e| format!("(no script: {e})"))
}

/// The job's form as the designer would send it back, changed.
async fn edit(s: &mut Box<dyn Session>, id: &str, change: Value) -> FormValues {
    let form = s.scheduler_form("job", Some(id)).await.unwrap();
    assert!(!form.read_only, "{:?}", form.notes);
    let mut v = values_of(&form);
    set(&mut v, change);
    v
}

async fn no_changes(s: &mut Box<dyn Session>, id: &str) {
    let v = edit(s, id, json!({})).await;
    let sc = script(s, Some(id), &v).await;
    assert!(sc.contains("no hay cambios"), "{id}: {sc}");
}

async fn items(s: &mut Box<dyn Session>) -> Vec<String> {
    s.scheduler_items("job")
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.id)
        .collect()
}

/// The actions the tree offers on a job now.
async fn item_actions(s: &mut Box<dyn Session>, id: &str) -> Option<Vec<String>> {
    s.scheduler_items("job")
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id == id)
        .unwrap_or_else(|| panic!("{id} isn't listed"))
        .actions
}

/// Waits until the tree offers `want` on the job.
async fn wait_actions(s: &mut Box<dyn Session>, id: &str, want: &[&str]) {
    let want: Vec<String> = want.iter().map(|a| a.to_string()).collect();
    for _ in 0..120 {
        if item_actions(s, id).await.as_ref() == Some(&want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("timed out: {id} offering {want:?}");
}

async fn wait_until<F, Fut>(what: &str, f: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..120 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("timed out: {what}");
}

async fn source_index(base: &str, name: &str) {
    let (st, b) = http(
        "PUT",
        &format!("{base}/{name}"),
        Some(json!({"mappings": {"properties": {"host": {"type": "keyword"}, "v": {"type": "double"}, "t": {"type": "date"}}}})),
    )
    .await;
    assert!(st < 300, "{b}");
    for (h, v) in [("a", 1), ("b", 2), ("a", 3)] {
        let (st, b) = http(
            "POST",
            &format!("{base}/{name}/_doc?refresh=true"),
            Some(json!({"host": h, "v": v, "t": "2026-11-12T10:00:00Z"})),
        )
        .await;
        assert!(st < 300, "{b}");
    }
}

/// The test indices (named after the prefix), deleted one by one.
async fn drop_indices(base: &str) -> usize {
    let (_, v) = http(
        "GET",
        &format!("{base}/_cat/indices/{PREFIX}*?format=json&expand_wildcards=all"),
        None,
    )
    .await;
    for i in v.as_array().into_iter().flatten() {
        if let Some(n) = i["index"].as_str() {
            http("DELETE", &format!("{base}/{n}"), None).await;
        }
    }
    let (_, v) = http(
        "GET",
        &format!("{base}/_cat/indices/{PREFIX}*?format=json&expand_wildcards=all"),
        None,
    )
    .await;
    v.as_array().map_or(0, Vec::len)
}

// ---------------------------------------------------------------- Elasticsearch

async fn es_cleanup(base: String) -> usize {
    let mut left = 0;
    let (_, v) = http(
        "GET",
        &format!("{base}/_transform/{PREFIX}*?size=1000"),
        None,
    )
    .await;
    for t in v["transforms"].as_array().into_iter().flatten() {
        let id = t["id"].as_str().unwrap();
        http(
            "DELETE",
            &format!("{base}/_transform/{id}?force=true"),
            None,
        )
        .await;
    }
    for w in ["dbine_test_sched_w", "dbine_test_sched_w2", SYS_WATCH] {
        http("DELETE", &format!("{base}/_watcher/watch/{w}"), None).await;
    }
    let (_, v) = http("GET", &format!("{base}/_slm/policy"), None).await;
    for (id, _) in v.as_object().into_iter().flatten() {
        if id.starts_with(PREFIX) {
            http("DELETE", &format!("{base}/_slm/policy/{id}"), None).await;
        }
    }
    let cloud_slm_ours = || async {
        let (st, v) = http("GET", &format!("{base}/_slm/policy/{CLOUD_SLM}"), None).await;
        st == 200 && v[CLOUD_SLM]["policy"]["config"]["metadata"]["dbine_test_sched"] == true
    };
    if cloud_slm_ours().await {
        http("DELETE", &format!("{base}/_slm/policy/{CLOUD_SLM}"), None).await;
    }
    let (st, _) = http(
        "GET",
        &format!("{base}/_snapshot/dbine_test_sched_repo"),
        None,
    )
    .await;
    if st == 200 {
        http(
            "DELETE",
            &format!("{base}/_snapshot/dbine_test_sched_repo/*"),
            None,
        )
        .await;
        http(
            "DELETE",
            &format!("{base}/_snapshot/dbine_test_sched_repo"),
            None,
        )
        .await;
    }
    http(
        "DELETE",
        &format!("{base}/_ingest/pipeline/dbine_test_sched_p"),
        None,
    )
    .await;
    let ilm = || async {
        let (_, v) = http("GET", &format!("{base}/_ilm/policy"), None).await;
        v.as_object()
            .map(|o| {
                o.keys()
                    .filter(|k| k.starts_with(PREFIX))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    for id in ilm().await {
        http("DELETE", &format!("{base}/_ilm/policy/{id}"), None).await;
    }
    // What the cluster recorded about them.
    for (index, field) in [
        (".watcher-history-*", "watch_id"),
        (".slm-history-*", "policy"),
        (".transform-notifications-*", "transform_id"),
    ] {
        http(
            "POST",
            &format!("{base}/{index}/_delete_by_query?ignore_unavailable=true&allow_no_indices=true&refresh=true&conflicts=proceed&expand_wildcards=all"),
            Some(json!({"query": {"prefix": {field: PREFIX}}})),
        )
        .await;
    }
    left += drop_indices(&base).await;
    let (_, v) = http("GET", &format!("{base}/_transform/{PREFIX}*"), None).await;
    left += v["count"].as_u64().unwrap_or(0) as usize;
    for w in ["dbine_test_sched_w", "dbine_test_sched_w2", SYS_WATCH] {
        let (st, _) = http("GET", &format!("{base}/_watcher/watch/{w}"), None).await;
        left += usize::from(st == 200);
    }
    let (_, v) = http("GET", &format!("{base}/_slm/policy"), None).await;
    left += v
        .as_object()
        .map_or(0, |o| o.keys().filter(|k| k.starts_with(PREFIX)).count());
    left += usize::from(cloud_slm_ours().await);
    let (st, _) = http(
        "GET",
        &format!("{base}/_snapshot/dbine_test_sched_repo"),
        None,
    )
    .await;
    left += usize::from(st == 200);
    left += ilm().await.len();
    left
}

async fn es_transforms(base: String) {
    let mut s = open("elasticsearch", &base).await;
    source_index(&base, "dbine_test_sched_src").await;
    let (st, b) = http(
        "PUT",
        &format!("{base}/_ingest/pipeline/dbine_test_sched_p"),
        Some(json!({"processors": [{"set": {"field": "via", "value": "p"}}]})),
    )
    .await;
    assert!(st < 300, "{b}");

    let info = s.scheduler_info().await.unwrap();
    let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "ilm"]);
    assert!(info.kinds[0].can_create && info.kinds[0].can_edit && info.kinds[0].can_delete);
    assert!(info.logs);
    // Watches start at an action; watches and SLM keep a history; a stopped
    // transform resets and a watch's actions are acknowledged.
    let job = &info.kinds[0];
    assert!(job.run_from_step && job.clear_history, "{job:?}");
    let acts: Vec<&str> = job.actions.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(acts, ["reset", "ack"]);

    // Create a continuous pivot transform, started.
    let v = new_job(
        &mut s,
        json!({
            "type": "transform",
            "id": "dbine_test_sched_tf",
            "description": "por host",
            "source_index": "dbine_test_sched_src",
            "group_by": "{\"host\": {\"terms\": {\"field\": \"host\"}}}",
            "aggregations": "{\"v\": {\"sum\": {\"field\": \"v\"}}}",
            "dest_index": "dbine_test_sched_dst",
            "dest_pipeline": "dbine_test_sched_p",
            "continuous": true,
            "sync_field": "t",
            "sync_delay": "30s",
            "frequency": "2m",
            "enabled": true,
            "max_page_search_size": 300
        }),
    )
    .await;
    let sc = script(&mut s, None, &v).await;
    assert!(sc.starts_with("# Transform «dbine_test_sched_tf»"), "{sc}");
    assert!(sc.contains("PUT /_transform/dbine_test_sched_tf\n"), "{sc}");
    assert!(
        sc.contains("POST /_transform/dbine_test_sched_tf/_start"),
        "{sc}"
    );
    let id = save(&mut s, None, &v).await;
    assert_eq!(id, "transform:dbine_test_sched_tf");
    assert!(items(&mut s).await.contains(&id));
    // Started: no reset; and a transform doesn't start at a step.
    assert_eq!(item_actions(&mut s, &id).await, Some(vec![]));
    let e = s.run_job_from(&id, "Transform").await.unwrap_err();
    assert!(e.to_string().contains("Ejecutar ahora"), "{e}");
    let (_, t) = http(
        "GET",
        &format!("{base}/_transform/dbine_test_sched_tf"),
        None,
    )
    .await;
    let t = &t["transforms"][0];
    assert_eq!(t["description"], "por host");
    assert_eq!(t["frequency"], "2m");
    assert_eq!(t["dest"]["pipeline"], "dbine_test_sched_p");
    assert_eq!(t["sync"]["time"]["delay"], "30s");
    assert_eq!(t["settings"]["max_page_search_size"], 300);
    no_changes(&mut s, &id).await;

    // In place: description, frequency, pipeline removed, retention, a
    // setting, _meta; then stop.
    let v = edit(
        &mut s,
        &id,
        json!({
            "description": "por host (2)",
            "frequency": "5m",
            "dest_pipeline": "",
            "retention_field": "t",
            "retention_max_age": "30d",
            "docs_per_second": 50,
            "meta": "{\"equipo\": \"dbine\"}",
            "enabled": false
        }),
    )
    .await;
    let sc = script(&mut s, Some(&id), &v).await;
    assert!(
        sc.contains("POST /_transform/dbine_test_sched_tf/_update"),
        "{sc}"
    );
    assert!(sc.contains("/_stop"), "{sc}");
    assert!(!sc.contains("DELETE"), "{sc}");
    save(&mut s, Some(&id), &v).await;
    let (_, t) = http(
        "GET",
        &format!("{base}/_transform/dbine_test_sched_tf"),
        None,
    )
    .await;
    let t = &t["transforms"][0];
    assert_eq!(t["description"], "por host (2)");
    assert_eq!(t["frequency"], "5m");
    assert!(t["dest"].get("pipeline").is_none(), "{t}");
    assert_eq!(t["retention_policy"]["time"]["max_age"], "30d");
    assert_eq!(t["settings"]["docs_per_second"], 50.0);
    assert_eq!(t["_meta"]["equipo"], "dbine");
    wait_until("stopped", || async {
        let (_, st) = http(
            "GET",
            &format!("{base}/_transform/dbine_test_sched_tf/_stats"),
            None,
        )
        .await;
        st["transforms"][0]["state"] == "stopped"
    })
    .await;
    no_changes(&mut s, &id).await;

    // Stopped, it resets: its checkpoints go, and so does the destination
    // index it created.
    wait_actions(&mut s, &id, &["reset"]).await;
    let sc = s
        .scheduler_action_script("job", &id, "reset", &Default::default())
        .await
        .unwrap();
    assert!(
        sc.contains("POST /_transform/dbine_test_sched_tf/_reset"),
        "{sc}"
    );
    let mut ro = ReadOnlySession::new(open("elasticsearch", &base).await);
    assert!(ro
        .scheduler_action_script("job", &id, "reset", &Default::default())
        .await
        .is_ok());
    assert!(ro
        .scheduler_action("job", &id, "reset", &Default::default())
        .await
        .is_err());
    assert!(s
        .scheduler_action_script("job", &id, "ack", &Default::default())
        .await
        .is_err());
    assert!(s
        .scheduler_action_script("job", &id, "nope", &Default::default())
        .await
        .is_err());
    assert!(s
        .scheduler_action_script("ilm", &id, "reset", &Default::default())
        .await
        .is_err());
    let (st, _) = http("GET", &format!("{base}/dbine_test_sched_dst"), None).await;
    assert_eq!(st, 200, "the destination index is there before the reset");
    s.scheduler_action("job", &id, "reset", &Default::default())
        .await
        .unwrap();
    let (st, _) = http("GET", &format!("{base}/dbine_test_sched_dst"), None).await;
    assert_eq!(
        st, 404,
        "the destination index it created goes with the reset"
    );
    let (_, st) = http(
        "GET",
        &format!("{base}/_transform/dbine_test_sched_tf/_stats"),
        None,
    )
    .await;
    assert_eq!(
        st["transforms"][0]["checkpointing"]["last"]["checkpoint"]
            .as_u64()
            .unwrap_or(0),
        0,
        "{st}"
    );

    // Remove the retention and the setting, in place.
    let v = edit(
        &mut s,
        &id,
        json!({"retention_field": "", "retention_max_age": "", "docs_per_second": ""}),
    )
    .await;
    save(&mut s, Some(&id), &v).await;
    let (_, t) = http(
        "GET",
        &format!("{base}/_transform/dbine_test_sched_tf"),
        None,
    )
    .await;
    let t = &t["transforms"][0];
    assert!(t.get("retention_policy").is_none(), "{t}");
    assert!(t["settings"].get("docs_per_second").is_none(), "{t}");

    // The pivot can't change in place: recreated.
    let v = edit(
        &mut s,
        &id,
        json!({"aggregations": "{\"v\": {\"max\": {\"field\": \"v\"}}}"}),
    )
    .await;
    let sc = script(&mut s, Some(&id), &v).await;
    assert!(sc.contains("POST /_transform/_preview"), "{sc}");
    assert!(
        sc.contains("DELETE /_transform/dbine_test_sched_tf?force=true"),
        "{sc}"
    );
    assert!(sc.contains("se pierden sus checkpoints"), "{sc}");
    save(&mut s, Some(&id), &v).await;
    let (_, t) = http(
        "GET",
        &format!("{base}/_transform/dbine_test_sched_tf"),
        None,
    )
    .await;
    assert_eq!(
        t["transforms"][0]["pivot"]["aggregations"]["v"]["max"]["field"],
        "v"
    );
    assert_eq!(t["transforms"][0]["description"], "por host (2)");

    // A latest transform, batch, then made continuous (recreated).
    let v = new_job(
        &mut s,
        json!({
            "type": "transform",
            "id": "dbine_test_sched_latest",
            "source_index": "dbine_test_sched_src",
            "function": "latest",
            "unique_key": "host",
            "sort": "t",
            "dest_index": "dbine_test_sched_latest_dst",
            "enabled": false
        }),
    )
    .await;
    let lid = save(&mut s, None, &v).await;
    let v = edit(&mut s, &lid, json!({"continuous": true, "sync_field": "t"})).await;
    let sc = script(&mut s, Some(&lid), &v).await;
    assert!(
        sc.contains("DELETE /_transform/dbine_test_sched_latest"),
        "{sc}"
    );
    save(&mut s, Some(&lid), &v).await;
    let (_, t) = http(
        "GET",
        &format!("{base}/_transform/dbine_test_sched_latest"),
        None,
    )
    .await;
    assert_eq!(t["transforms"][0]["sync"]["time"]["field"], "t");
    assert_eq!(t["transforms"][0]["latest"]["unique_key"][0], "host");

    // A new id: the new one exists, the old one is gone.
    let v = edit(&mut s, &lid, json!({"id": "dbine_test_sched_latest2"})).await;
    let new_id = save(&mut s, Some(&lid), &v).await;
    assert_eq!(new_id, "transform:dbine_test_sched_latest2");
    let ids = items(&mut s).await;
    assert!(ids.contains(&new_id) && !ids.contains(&lid), "{ids:?}");

    // Bad values are refused before the server.
    for (k, x) in [
        ("id", json!("Bad Id")),
        ("group_by", json!("{not json")),
        ("frequency", json!("5 minutes")),
        ("dest_index", json!("")),
    ] {
        let mut v = edit(&mut s, &id, json!({})).await;
        set(&mut v, json!({ k: x }));
        assert!(
            s.scheduler_script("job", Some(&id), &v).await.is_err(),
            "{k}"
        );
    }
    assert!(s
        .scheduler_form("job", Some("transform:Nope"))
        .await
        .is_err());
    assert!(s.scheduler_form("schedule", None).await.is_err());

    // Read-only: forms and scripts, no saves or deletes.
    let mut ro = ReadOnlySession::new(open("elasticsearch", &base).await);
    let form = ro.scheduler_form("job", Some(&id)).await.unwrap();
    let v = values_of(&form);
    assert!(ro.scheduler_script("job", Some(&id), &v).await.is_ok());
    assert!(ro.scheduler_save("job", Some(&id), &v).await.is_err());
    assert!(ro.scheduler_delete("job", &id).await.is_err());

    // The transforms' messages.
    let logs = s.scheduler_logs().await.unwrap();
    let msgs = logs.iter().find(|t| t.key == "transform_messages").unwrap();
    assert!(msgs
        .rows
        .iter()
        .any(|r| r[1] == json!("dbine_test_sched_tf")));

    // Delete.
    for id in [id.as_str(), new_id.as_str()] {
        let sc = s.scheduler_delete_script("job", id).await.unwrap();
        assert!(sc.contains("DELETE /_transform/"), "{sc}");
        s.scheduler_delete("job", id).await.unwrap();
    }
    let ids = items(&mut s).await;
    assert!(!ids.iter().any(|i| i.contains(PREFIX)), "{ids:?}");
}

async fn es_watches(base: String) {
    let mut s = open("elasticsearch", &base).await;
    source_index(&base, "dbine_test_sched_src").await;
    let form = s.scheduler_form("job", None).await.unwrap();
    let ty = form.sections[0]
        .fields
        .iter()
        .find(|f| f.key == "type")
        .unwrap();
    let opts = serde_json::to_value(&ty.kind).unwrap();
    assert_eq!(opts["options"].as_array().unwrap().len(), 3, "{opts}");

    // In this order (a serde_json value would sort the keys).
    let actions = r#"{
  "uno": {"logging": {"text": "uno"}},
  "hook": {"webhook": {"method": "POST", "host": "localhost", "port": 9200,
    "path": "/dbine_test_sched_src/_doc", "body": "{\"host\": \"w\"}",
    "auth": {"basic": {"username": "u", "password": "clave-secreta"}}}},
  "dos": {"logging": {"text": "dos"}}
}"#;
    let v = new_job(
        &mut s,
        json!({
            "type": "watch",
            "id": "dbine_test_sched_w",
            "description": "Watch de prueba",
            "metadata": "{\"umbral\": 3}",
            "enabled": false,
            "trigger_kind": "weekly",
            "week_days": ["monday", "friday"],
            "times": "09:00, 17:30",
            "input": "{\"search\": {\"request\": {\"indices\": [\"dbine_test_sched_src\"], \"body\": {\"query\": {\"match_all\": {}}}}}}",
            "condition": "{\"compare\": {\"ctx.payload.hits.total\": {\"gte\": 1}}}",
            "actions": actions,
            "throttle_period": "10m"
        }),
    )
    .await;
    let sc = script(&mut s, None, &v).await;
    assert!(
        sc.contains("PUT /_watcher/watch/dbine_test_sched_w?active=false"),
        "{sc}"
    );
    assert!(
        sc.contains(SECRET_MASK) && !sc.contains("clave-secreta"),
        "{sc}"
    );
    let id = save(&mut s, None, &v).await;
    assert_eq!(id, "watch:dbine_test_sched_w");
    let (_, w) = http(
        "GET",
        &format!("{base}/_watcher/watch/dbine_test_sched_w"),
        None,
    )
    .await;
    assert_eq!(w["status"]["state"]["active"], false);
    assert_eq!(w["watch"]["metadata"]["name"], "Watch de prueba");
    assert_eq!(w["watch"]["metadata"]["umbral"], 3);
    assert_eq!(w["watch"]["throttle_period_in_millis"], 600000);
    assert_eq!(
        w["watch"]["trigger"]["schedule"]["weekly"][0]["at"],
        json!(["09:00", "17:30"])
    );
    let form = s.scheduler_form("job", Some(&id)).await.unwrap();
    assert_eq!(form.values["trigger_kind"], "weekly");
    assert_eq!(form.values["week_days"], json!(["monday", "friday"]));
    no_changes(&mut s, &id).await;

    // In place, the redacted password kept: values only (the times, the
    // condition's threshold, the throttle), and active.
    let v = edit(
        &mut s,
        &id,
        json!({
            "times": "08:15",
            "condition": "{\"compare\": {\"ctx.payload.hits.total\": {\"gte\": 2}}}",
            "enabled": true,
            "throttle_period": "1m"
        }),
    )
    .await;
    let sc = script(&mut s, Some(&id), &v).await;
    assert!(sc.contains("if_seq_no="), "{sc}");
    assert!(sc.contains("::es_redacted::"), "{sc}");
    save(&mut s, Some(&id), &v).await;
    let (_, w) = http(
        "GET",
        &format!("{base}/_watcher/watch/dbine_test_sched_w"),
        None,
    )
    .await;
    assert_eq!(w["status"]["state"]["active"], true);
    assert_eq!(
        w["watch"]["trigger"]["schedule"]["weekly"][0]["at"],
        json!(["08:15"])
    );
    assert_eq!(w["watch"]["throttle_period_in_millis"], 60000);
    assert_eq!(
        w["watch"]["condition"]["compare"]["ctx.payload.hits.total"]["gte"],
        2
    );
    let (_, raw) = http(
        "POST",
        &format!("{base}/.watches/_search"),
        Some(json!({"query": {"ids": {"values": ["dbine_test_sched_w"]}}})),
    )
    .await;
    let stored = &raw["hits"]["hits"][0]["_source"]["actions"]["hook"]["webhook"]["auth"]["basic"]
        ["password"];
    assert!(
        stored.as_str().is_some_and(|p| p != "::es_redacted::"),
        "{stored}"
    );
    no_changes(&mut s, &id).await;

    // Only "active": _deactivate.
    let v = edit(&mut s, &id, json!({"enabled": false})).await;
    let sc = script(&mut s, Some(&id), &v).await;
    assert!(
        sc.contains("PUT /_watcher/watch/dbine_test_sched_w/_deactivate"),
        "{sc}"
    );
    save(&mut s, Some(&id), &v).await;

    // Start at the third action: the first two skipped.
    s.run_job_from(&id, "dos").await.unwrap();
    assert!(s.run_job_from(&id, "nope").await.is_err());
    wait_until("a recorded run", || async {
        let (_, h) = http(
            "POST",
            &format!("{base}/.watcher-history-*/_search?ignore_unavailable=true"),
            Some(json!({"query": {"term": {"watch_id": "dbine_test_sched_w"}}})),
        )
        .await;
        h["hits"]["total"]["value"].as_u64().unwrap_or(0) > 0
    })
    .await;
    let runs = s.job_runs(&id, 10).await.unwrap();
    let run = &runs[0];
    let step = |n: &str| run.steps.iter().find(|x| x.name == n).map(|x| x.outcome);
    assert_eq!(
        step("dos"),
        Some(dbine_driver::JobOutcome::Succeeded),
        "{run:?}"
    );
    assert_ne!(
        step("uno"),
        Some(dbine_driver::JobOutcome::Succeeded),
        "{run:?}"
    );

    // The actions that ran wait for an acknowledgement: acked, Watcher
    // throttles them, and there's nothing left to acknowledge.
    wait_actions(&mut s, &id, &["ack"]).await;
    let sc = s
        .scheduler_action_script("job", &id, "ack", &Default::default())
        .await
        .unwrap();
    assert!(
        sc.contains("PUT /_watcher/watch/dbine_test_sched_w/_ack"),
        "{sc}"
    );
    assert!(s
        .scheduler_action_script("job", &id, "reset", &Default::default())
        .await
        .is_err());
    s.scheduler_action("job", &id, "ack", &Default::default())
        .await
        .unwrap();
    let (_, w) = http(
        "GET",
        &format!("{base}/_watcher/watch/dbine_test_sched_w"),
        None,
    )
    .await;
    assert_eq!(
        w["status"]["actions"]["dos"]["ack"]["state"], "acked",
        "{w}"
    );
    assert_eq!(item_actions(&mut s, &id).await, Some(vec![]));
    assert!(s
        .scheduler_action("job", &id, "ack", &Default::default())
        .await
        .is_err());

    // Clear the history.
    s.clear_job_history(&id).await.unwrap();
    assert!(s.job_runs(&id, 10).await.unwrap().is_empty());

    // The log.
    let logs = s.scheduler_logs().await.unwrap();
    assert!(logs.iter().any(|t| t.key == "watcher_history"));

    // Another kind of schedule changes the definition's shape: with the
    // password still redacted it's refused; typed again, the watch is
    // replaced whole.
    let change = json!({"trigger_kind": "cron", "cron": "0 0/30 * * * ?"});
    let v = edit(&mut s, &id, change.clone()).await;
    let e = s.scheduler_script("job", Some(&id), &v).await.unwrap_err();
    assert!(e.to_string().contains("::es_redacted::"), "{e}");
    let mut v = edit(&mut s, &id, change).await;
    set(&mut v, json!({"actions": actions}));
    let sc = script(&mut s, Some(&id), &v).await;
    assert!(
        !sc.contains("if_seq_no") && sc.contains(SECRET_MASK),
        "{sc}"
    );
    save(&mut s, Some(&id), &v).await;
    let (_, w) = http(
        "GET",
        &format!("{base}/_watcher/watch/dbine_test_sched_w"),
        None,
    )
    .await;
    assert_eq!(
        w["watch"]["trigger"]["schedule"],
        json!({"cron": "0 0/30 * * * ?"})
    );

    // A JSON schedule, kept as JSON (the password typed again).
    let mut v = edit(
        &mut s,
        &id,
        json!({"trigger_kind": "json", "trigger_json": "{\"weekly\": [{\"on\": [\"monday\"], \"at\": [\"10:00\"]}, {\"on\": [\"friday\"], \"at\": [\"18:00\"]}]}"}),
    )
    .await;
    set(&mut v, json!({"actions": actions}));
    save(&mut s, Some(&id), &v).await;
    let form = s.scheduler_form("job", Some(&id)).await.unwrap();
    assert_eq!(form.values["trigger_kind"], "json", "{:?}", form.values);
    let (_, w) = http(
        "GET",
        &format!("{base}/_watcher/watch/dbine_test_sched_w"),
        None,
    )
    .await;
    assert!(
        w["watch"]["trigger"]["schedule"].get("cron").is_none(),
        "{w}"
    );
    no_changes(&mut s, &id).await;

    // A new id with a redacted value is refused; with the password typed,
    // a new watch and the old one gone.
    let v = edit(&mut s, &id, json!({"id": "dbine_test_sched_w2"})).await;
    assert!(s.scheduler_save("job", Some(&id), &v).await.is_err());
    let v = edit(
        &mut s,
        &id,
        json!({"id": "dbine_test_sched_w2", "actions": actions}),
    )
    .await;
    let new_id = save(&mut s, Some(&id), &v).await;
    assert_eq!(new_id, "watch:dbine_test_sched_w2");
    let ids = items(&mut s).await;
    assert!(ids.contains(&new_id) && !ids.contains(&id), "{ids:?}");

    // Delete.
    let sc = s.scheduler_delete_script("job", &new_id).await.unwrap();
    assert!(
        sc.contains("DELETE /_watcher/watch/dbine_test_sched_w2"),
        "{sc}"
    );
    s.scheduler_delete("job", &new_id).await.unwrap();
    assert!(!items(&mut s).await.contains(&new_id));

    // A watch of Monitoring's (Elasticsearch creates and keeps them):
    // listed and shown, but neither changed, deleted nor acknowledged.
    let (st, b) = http(
        "PUT",
        &format!("{base}/_watcher/watch/{SYS_WATCH}?active=false"),
        Some(json!({
            "trigger": {"schedule": {"interval": "1h"}},
            "input": {"simple": {}},
            "actions": {"registro": {"logging": {"text": "x"}}},
            "metadata": {"xpack": {"type": "monitoring"}}
        })),
    )
    .await;
    assert!(st < 300, "{b}");
    let sys = format!("watch:{SYS_WATCH}");
    let item = s
        .scheduler_items("job")
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id == sys)
        .expect("the system watch listed");
    assert!(item.system);
    assert_eq!((item.can_edit, item.can_delete), (Some(false), Some(false)));
    assert_eq!(item.actions, Some(vec![]));
    let form = s.scheduler_form("job", Some(&sys)).await.unwrap();
    assert!(form.read_only, "{:?}", form.notes);
    assert!(form.notes[0].contains("Monitoring"), "{:?}", form.notes);
    let mut v = values_of(&form);
    set(&mut v, json!({"description": "cambiada"}));
    for e in [
        refusal(s.scheduler_script("job", Some(&sys), &v).await),
        refusal(s.scheduler_save("job", Some(&sys), &v).await),
        refusal(s.scheduler_delete_script("job", &sys).await),
        refusal(s.scheduler_delete("job", &sys).await),
        refusal(
            s.scheduler_action_script("job", &sys, "ack", &Default::default())
                .await,
        ),
        refusal(
            s.scheduler_action("job", &sys, "ack", &Default::default())
                .await,
        ),
    ] {
        assert!(e.contains("Monitoring"), "{e}");
    }
    let (st, w) = http("GET", &format!("{base}/_watcher/watch/{SYS_WATCH}"), None).await;
    assert_eq!(st, 200);
    assert!(w["watch"]["metadata"].get("name").is_none(), "{w}");
    http(
        "DELETE",
        &format!("{base}/_watcher/watch/{SYS_WATCH}"),
        None,
    )
    .await;
}

async fn es_slm(base: String) {
    let mut s = open("elasticsearch", &base).await;
    source_index(&base, "dbine_test_sched_src").await;
    let (st, b) = http(
        "PUT",
        &format!("{base}/_snapshot/dbine_test_sched_repo"),
        Some(json!({"type": "fs", "settings": {"location": "/tmp/snaps/dbine_test_sched_repo"}})),
    )
    .await;
    assert!(st < 300, "{b}");
    let form = s.scheduler_form("job", None).await.unwrap();
    assert!(form.choices["repository"]
        .iter()
        .any(|(r, _)| r == "dbine_test_sched_repo"));

    let v = new_job(
        &mut s,
        json!({
            "type": "slm",
            "id": "dbine_test_sched_slm",
            "snapshot_name": "<dbine-test-sched-{now/d}>",
            "repository": "dbine_test_sched_repo",
            "slm_schedule": "0 30 1 * * ?",
            "indices": "dbine_test_sched_src",
            "include_global_state": false,
            "snapshot_metadata": "{\"por\": \"dbine\"}",
            "expire_after": "30d",
            "min_count": 1,
            "max_count": 5
        }),
    )
    .await;
    let sc = script(&mut s, None, &v).await;
    assert!(sc.contains("PUT /_slm/policy/dbine_test_sched_slm"), "{sc}");
    let id = save(&mut s, None, &v).await;
    assert_eq!(id, "slm:dbine_test_sched_slm");
    let (_, p) = http(
        "GET",
        &format!("{base}/_slm/policy/dbine_test_sched_slm"),
        None,
    )
    .await;
    let p = &p["dbine_test_sched_slm"]["policy"];
    assert_eq!(p["schedule"], "0 30 1 * * ?");
    assert_eq!(p["config"]["include_global_state"], false);
    assert_eq!(p["retention"]["max_count"], 5);
    no_changes(&mut s, &id).await;

    // A repository that doesn't exist is refused before the server.
    let v = edit(&mut s, &id, json!({"repository": "nope"})).await;
    assert!(s.scheduler_script("job", Some(&id), &v).await.is_err());

    // In place: schedule and retention.
    let v = edit(
        &mut s,
        &id,
        json!({"slm_schedule": "0 0 2 * * ?", "max_count": 3, "expire_after": ""}),
    )
    .await;
    save(&mut s, Some(&id), &v).await;
    let (_, p) = http(
        "GET",
        &format!("{base}/_slm/policy/dbine_test_sched_slm"),
        None,
    )
    .await;
    let p = &p["dbine_test_sched_slm"]["policy"];
    assert_eq!(p["schedule"], "0 0 2 * * ?");
    assert_eq!(p["retention"]["max_count"], 3);
    assert!(p["retention"].get("expire_after").is_none(), "{p}");

    // Run it (the job's run) and read its history; start at its step.
    s.run_job(&id).await.unwrap();
    wait_until("an SLM history record", || async {
        let (_, h) = http(
            "POST",
            &format!("{base}/.slm-history-*/_search?ignore_unavailable=true"),
            Some(json!({"query": {"term": {"policy": "dbine_test_sched_slm"}}})),
        )
        .await;
        h["hits"]["total"]["value"].as_u64().unwrap_or(0) > 0
    })
    .await;
    let runs = s.job_runs(&id, 10).await.unwrap();
    assert!(!runs.is_empty());
    // A policy has one step: it runs whole, "Ejecutar ahora".
    let e = s.run_job_from(&id, "Snapshot").await.unwrap_err();
    assert!(e.to_string().contains("Ejecutar ahora"), "{e}");
    assert_eq!(item_actions(&mut s, &id).await, Some(vec![]));
    let logs = s.scheduler_logs().await.unwrap();
    assert!(logs
        .iter()
        .any(|t| t.key == "slm_history"
            && t.rows.iter().any(|r| r[1] == json!("dbine_test_sched_slm"))));
    s.clear_job_history(&id).await.unwrap();

    // A new id; then delete.
    let v = edit(&mut s, &id, json!({"id": "dbine_test_sched_slm2"})).await;
    let new_id = save(&mut s, Some(&id), &v).await;
    let ids = items(&mut s).await;
    assert!(ids.contains(&new_id) && !ids.contains(&id), "{ids:?}");
    s.scheduler_delete("job", &new_id).await.unwrap();
    assert!(!items(&mut s).await.contains(&new_id));

    // Elastic Cloud's own policy (made here as Cloud makes it, scheduled
    // far ahead so it never runs): its schedule changes in place; its id,
    // its repository and the policy itself stay.
    let (st, _) = http("GET", &format!("{base}/_slm/policy/{CLOUD_SLM}"), None).await;
    if st != 404 {
        eprintln!("the cluster already has a {CLOUD_SLM}: its checks skipped");
        return;
    }
    let (st, b) = http(
        "PUT",
        &format!("{base}/_slm/policy/{CLOUD_SLM}"),
        Some(json!({
            "name": "<cloud-snapshot-{now/d}>",
            "schedule": "0 0 0 1 1 ? 2099",
            "repository": "dbine_test_sched_repo",
            "config": {"partial": true, "metadata": {"dbine_test_sched": true}},
            "retention": {"expire_after": "30d", "min_count": 1, "max_count": 10}
        })),
    )
    .await;
    assert!(st < 300, "{b}");
    let cloud = format!("slm:{CLOUD_SLM}");
    let item = s
        .scheduler_items("job")
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id == cloud)
        .expect("Elastic Cloud's policy listed");
    assert_eq!((item.can_edit, item.can_delete), (None, Some(false)));
    let form = s.scheduler_form("job", Some(&cloud)).await.unwrap();
    assert!(!form.read_only, "{:?}", form.notes);
    assert!(form.notes[0].contains("Elastic Cloud"), "{:?}", form.notes);
    let locked: Vec<&str> = form
        .sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .filter(|f| f.read_only)
        .map(|f| f.key.as_str())
        .collect();
    assert_eq!(locked, ["type", "id", "repository"]);
    for change in [
        json!({"id": "dbine_test_sched_cloud"}),
        json!({"repository": "otro"}),
    ] {
        let v = edit(&mut s, &cloud, change.clone()).await;
        for e in [
            refusal(s.scheduler_script("job", Some(&cloud), &v).await),
            refusal(s.scheduler_save("job", Some(&cloud), &v).await),
        ] {
            assert!(e.contains("Elastic Cloud"), "{change}: {e}");
        }
    }
    for e in [
        refusal(s.scheduler_delete_script("job", &cloud).await),
        refusal(s.scheduler_delete("job", &cloud).await),
    ] {
        assert!(e.contains("Elastic Cloud"), "{e}");
    }
    let v = edit(
        &mut s,
        &cloud,
        json!({"slm_schedule": "0 0 0 2 1 ? 2099", "max_count": 20}),
    )
    .await;
    let sc = script(&mut s, Some(&cloud), &v).await;
    assert!(
        sc.contains(&format!("PUT /_slm/policy/{CLOUD_SLM}")) && !sc.contains("DELETE"),
        "{sc}"
    );
    save(&mut s, Some(&cloud), &v).await;
    let (_, p) = http("GET", &format!("{base}/_slm/policy/{CLOUD_SLM}"), None).await;
    let p = &p[CLOUD_SLM]["policy"];
    assert_eq!(p["schedule"], "0 0 0 2 1 ? 2099");
    assert_eq!(p["retention"]["max_count"], 20);
    assert_eq!(p["repository"], "dbine_test_sched_repo");
    assert_eq!(p["config"]["metadata"]["dbine_test_sched"], true);
    http("DELETE", &format!("{base}/_slm/policy/{CLOUD_SLM}"), None).await;
}

async fn es_ilm(base: String) {
    let mut s = open("elasticsearch", &base).await;
    let info = s.scheduler_info().await.unwrap();
    assert!(
        info.kinds.iter().any(|k| k.key == "ilm" && k.can_create),
        "{info:?}"
    );
    // The policies Elastic manages are listed as system ones, read only.
    let list = s.scheduler_items("ilm").await.unwrap();
    let managed = list
        .items
        .iter()
        .find(|i| i.system)
        .expect("a managed policy");
    assert_eq!(
        (managed.can_edit, managed.can_delete),
        (Some(false), Some(false))
    );
    let form = s.scheduler_form("ilm", Some(&managed.id)).await.unwrap();
    assert!(form.read_only);
    assert!(s.scheduler_delete("ilm", &managed.id).await.is_err());

    let form = s.scheduler_form("ilm", None).await.unwrap();
    let mut v = values_of(&form);
    set(
        &mut v,
        json!({"id": "dbine_test_sched_ilm", "meta": "{\"description\": \"de prueba\"}"}),
    );
    let id = s.scheduler_save("ilm", None, &v).await.unwrap().unwrap();
    assert_eq!(id, "dbine_test_sched_ilm");
    let form = s.scheduler_form("ilm", Some(&id)).await.unwrap();
    let v = values_of(&form);
    assert!(s
        .scheduler_script("ilm", Some(&id), &v)
        .await
        .unwrap()
        .contains("no hay cambios"));
    let mut v2 = v.clone();
    set(
        &mut v2,
        json!({"phases": "{\"delete\": {\"min_age\": \"7d\", \"actions\": {\"delete\": {}}}}"}),
    );
    s.scheduler_save("ilm", Some(&id), &v2).await.unwrap();
    let (_, got) = http("GET", &format!("{base}/_ilm/policy/{id}"), None).await;
    assert_eq!(got[&id]["policy"]["phases"]["delete"]["min_age"], "7d");
    assert!(got[&id]["policy"]["phases"].get("hot").is_none());
    assert_eq!(got[&id]["version"], 2);
    let mut v3 = v2.clone();
    set(&mut v3, json!({"id": "dbine_test_sched_ilm2"}));
    let id2 = s
        .scheduler_save("ilm", Some(&id), &v3)
        .await
        .unwrap()
        .unwrap();
    let list = s.scheduler_items("ilm").await.unwrap();
    assert!(list.items.iter().any(|i| i.id == id2) && !list.items.iter().any(|i| i.id == id));
    s.scheduler_delete("ilm", &id2).await.unwrap();
    assert!(s.scheduler_form("ism", None).await.is_err());
}

#[tokio::test]
#[ignore]
async fn elasticsearch_ilm_policies() {
    let Some(base) = url("DBINE_TEST_ES_SCHED_URL") else {
        return;
    };
    with_cleanup(es_ilm(base.clone()), || es_cleanup(base)).await;
}

#[tokio::test]
#[ignore]
async fn elasticsearch_transforms() {
    let Some(base) = url("DBINE_TEST_ES_SCHED_URL") else {
        return;
    };
    with_cleanup(es_transforms(base.clone()), || es_cleanup(base)).await;
}

#[tokio::test]
#[ignore]
async fn elasticsearch_watches() {
    let Some(base) = url("DBINE_TEST_ES_SCHED_URL") else {
        return;
    };
    with_cleanup(es_watches(base.clone()), || es_cleanup(base)).await;
}

#[tokio::test]
#[ignore]
async fn elasticsearch_snapshot_policies() {
    let Some(base) = url("DBINE_TEST_ES_SCHED_URL") else {
        return;
    };
    with_cleanup(es_slm(base.clone()), || es_cleanup(base)).await;
}

/// A basic license: transforms and SLM, no Watcher.
#[tokio::test]
#[ignore]
async fn elasticsearch_basic_license() {
    let Some(base) = url("DBINE_TEST_ELASTICSEARCH_URL") else {
        return;
    };
    let b = base.clone();
    let body = async move {
        let mut s = open("elasticsearch", &b).await;
        let info = s.scheduler_info().await.unwrap();
        assert!(info.notes.iter().any(|n| n.contains("Watcher")), "{info:?}");
        // Without Watcher nothing starts at a step; SLM keeps a history.
        let job = &info.kinds[0];
        assert!(!job.run_from_step && job.clear_history, "{job:?}");
        let acts: Vec<&str> = job.actions.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(acts, ["reset"]);
        let form = s.scheduler_form("job", None).await.unwrap();
        let ty = &form.sections[0].fields[0];
        let opts = serde_json::to_value(&ty.kind).unwrap();
        let keys: Vec<&str> = opts["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o[0].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["transform", "slm"]);
        // A watch is refused.
        let mut v = values_of(&form);
        set(&mut v, json!({"type": "watch", "id": "dbine_test_sched_w"}));
        assert!(s.scheduler_save("job", None, &v).await.is_err());
        // A batch transform, created stopped, then deleted.
        source_index(&b, "dbine_test_sched_src").await;
        set(
            &mut v,
            json!({
                "type": "transform",
                "id": "dbine_test_sched_basic",
                "source_index": "dbine_test_sched_src",
                "group_by": "{\"host\": {\"terms\": {\"field\": \"host\"}}}",
                "aggregations": "{\"n\": {\"value_count\": {\"field\": \"v\"}}}",
                "dest_index": "dbine_test_sched_basic_dst",
                "enabled": false
            }),
        );
        let id = save(&mut s, None, &v).await;
        no_changes(&mut s, &id).await;
        s.scheduler_delete("job", &id).await.unwrap();
    };
    with_cleanup(body, || es_cleanup(base)).await;
}

// ------------------------------------------------------------- OpenSearch

async fn os_cleanup(base: String, p: &'static str) -> usize {
    let (_, v) = http("GET", &format!("{base}/{p}/_transform?size=1000"), None).await;
    for t in v["transforms"].as_array().into_iter().flatten() {
        if let Some(id) = t["_id"].as_str().filter(|i| i.starts_with(PREFIX)) {
            http("POST", &format!("{base}/{p}/_transform/{id}/_stop"), None).await;
            http("DELETE", &format!("{base}/{p}/_transform/{id}"), None).await;
        }
    }
    let monitors = || async {
        let (_, v) = http(
            "POST",
            &format!("{base}/{p}/_alerting/monitors/_search"),
            Some(json!({"size": 1000, "query": {"match_all": {}}})),
        )
        .await;
        v["hits"]["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|h| {
                let m = h["_source"].get("monitor").unwrap_or(&h["_source"]);
                m["name"].as_str().is_some_and(|n| n.starts_with(PREFIX))
            })
            .filter_map(|h| h["_id"].as_str().map(str::to_string))
            .collect::<Vec<_>>()
    };
    for id in monitors().await {
        http(
            "DELETE",
            &format!("{base}/{p}/_alerting/monitors/{id}"),
            None,
        )
        .await;
    }
    let policies = || async {
        let (_, v) = http("GET", &format!("{base}/{p}/_sm/policies"), None).await;
        v["policies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x["sm_policy"]["name"].as_str().map(str::to_string))
            .filter(|n| n.starts_with(PREFIX))
            .collect::<Vec<_>>()
    };
    for n in policies().await {
        http("DELETE", &format!("{base}/{p}/_sm/policies/{n}"), None).await;
    }
    let ism = || async {
        let (_, v) = http("GET", &format!("{base}/{p}/_ism/policies?size=1000"), None).await;
        v["policies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x["_id"].as_str().map(str::to_string))
            .filter(|n| n.starts_with(PREFIX))
            .collect::<Vec<_>>()
    };
    for n in ism().await {
        http("DELETE", &format!("{base}/{p}/_ism/policies/{n}"), None).await;
    }
    let mut left = drop_indices(&base).await;
    let (_, v) = http("GET", &format!("{base}/{p}/_transform?size=1000"), None).await;
    left += v["transforms"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["_id"].as_str().is_some_and(|i| i.starts_with(PREFIX)))
        .count();
    left += monitors().await.len();
    left += policies().await.len();
    left += ism().await.len();
    left
}

async fn os_jobs(base: String) {
    let mut s = open("opensearch", &base).await;
    source_index(&base, "dbine_test_sched_src").await;
    let info = s.scheduler_info().await.unwrap();
    assert_eq!(info.kinds[0].key, "job");
    // The switch and "run now" are all OpenSearch's jobs have.
    let job = &info.kinds[0];
    assert!(
        !job.run_from_step && !job.clear_history && job.actions.is_empty(),
        "{job:?}"
    );
    let form = s.scheduler_form("job", None).await.unwrap();
    let opts = serde_json::to_value(&form.sections[0].fields[0].kind).unwrap();
    assert_eq!(opts["options"].as_array().unwrap().len(), 3, "{opts}");
    // Each type's own tabs, shown for it only.
    let tabs: Vec<(&str, Option<&str>)> = form
        .sections
        .iter()
        .map(|t| (t.key.as_str(), t.when.as_ref().map(|w| w.1[0].as_str())))
        .collect();
    assert_eq!(tabs[0], ("general", None));
    assert!(
        tabs[1..]
            .iter()
            .all(|(k, w)| w.is_some_and(|w| k.starts_with(w))),
        "{tabs:?}"
    );
    assert!(
        tabs.contains(&("monitor_inputs", Some("monitor"))),
        "{tabs:?}"
    );

    // ---- transform
    let v = new_job(
        &mut s,
        json!({
            "type": "transform",
            "id": "dbine_test_sched_tf",
            "description": "por host",
            "source_index": "dbine_test_sched_src",
            "groups": "[{\"terms\": {\"source_field\": \"host\", \"target_field\": \"host\"}}]",
            "aggregations": "{\"v\": {\"sum\": {\"field\": \"v\"}}}",
            "dest_index": "dbine_test_sched_dst",
            "enabled": false,
            "interval": 5,
            "interval_unit": "hours",
            "page_size": 50
        }),
    )
    .await;
    let sc = script(&mut s, None, &v).await;
    assert!(
        sc.contains("PUT /_plugins/_transform/dbine_test_sched_tf"),
        "{sc}"
    );
    let id = save(&mut s, None, &v).await;
    assert_eq!(id, "transform:dbine_test_sched_tf");
    no_changes(&mut s, &id).await;
    // In place: description, schedule (to cron), page size, enabled.
    let v = edit(
        &mut s,
        &id,
        json!({"description": "otra", "schedule_kind": "cron", "cron": "0 * * * *", "timezone": "UTC", "page_size": 20, "enabled": true}),
    )
    .await;
    let sc = script(&mut s, Some(&id), &v).await;
    assert!(sc.contains("if_seq_no="), "{sc}");
    assert!(!sc.contains("DELETE"), "{sc}");
    save(&mut s, Some(&id), &v).await;
    let (_, t) = http(
        "GET",
        &format!("{base}/_plugins/_transform/dbine_test_sched_tf"),
        None,
    )
    .await;
    assert_eq!(t["transform"]["description"], "otra");
    assert_eq!(
        t["transform"]["schedule"]["cron"]["expression"],
        "0 * * * *"
    );
    assert_eq!(t["transform"]["page_size"], 20);
    assert_eq!(t["transform"]["enabled"], true);
    no_changes(&mut s, &id).await;
    // The target index can't change in place: recreated.
    let v = edit(&mut s, &id, json!({"dest_index": "dbine_test_sched_dst2"})).await;
    let sc = script(&mut s, Some(&id), &v).await;
    assert!(
        sc.contains("DELETE /_plugins/_transform/dbine_test_sched_tf"),
        "{sc}"
    );
    save(&mut s, Some(&id), &v).await;
    let (_, t) = http(
        "GET",
        &format!("{base}/_plugins/_transform/dbine_test_sched_tf"),
        None,
    )
    .await;
    assert_eq!(t["transform"]["target_index"], "dbine_test_sched_dst2");
    assert_eq!(t["transform"]["description"], "otra");
    // A new id.
    let v = edit(&mut s, &id, json!({"id": "dbine_test_sched_tf2"})).await;
    let tid = save(&mut s, Some(&id), &v).await;
    let ids = items(&mut s).await;
    assert!(ids.contains(&tid) && !ids.contains(&id), "{ids:?}");

    // ---- monitor
    let triggers = json!([{"query_level_trigger": {"name": "nunca", "severity": "1",
        "condition": {"script": {"source": "return false", "lang": "painless"}}, "actions": []}}]);
    let v = new_job(
        &mut s,
        json!({
            "type": "monitor",
            "name": "dbine_test_sched_mon",
            "monitor_type": "query_level_monitor",
            "enabled": false,
            "m_schedule_kind": "period",
            "m_interval": 10,
            "m_interval_unit": "minutes",
            "inputs_query": "[{\"search\": {\"indices\": [\"dbine_test_sched_src\"], \"query\": {\"size\": 0, \"query\": {\"match_all\": {}}}}}]",
            "triggers_query": triggers.to_string()
        }),
    )
    .await;
    let sc = script(&mut s, None, &v).await;
    assert!(sc.contains("POST /_plugins/_alerting/monitors"), "{sc}");
    let mid = save(&mut s, None, &v).await;
    assert!(mid.starts_with("monitor:"), "{mid}");
    assert!(items(&mut s).await.contains(&mid));
    no_changes(&mut s, &mid).await;
    let v = edit(
        &mut s,
        &mid,
        json!({"m_schedule_kind": "cron", "m_cron": "0 */6 * * *", "m_timezone": "UTC", "name": "dbine_test_sched_mon2"}),
    )
    .await;
    let sc = script(&mut s, Some(&mid), &v).await;
    assert!(sc.contains("PUT /_plugins/_alerting/monitors/"), "{sc}");
    save(&mut s, Some(&mid), &v).await;
    let raw = mid.trim_start_matches("monitor:");
    let (_, m) = http(
        "GET",
        &format!("{base}/_plugins/_alerting/monitors/{raw}"),
        None,
    )
    .await;
    assert_eq!(m["monitor"]["name"], "dbine_test_sched_mon2");
    assert_eq!(
        m["monitor"]["schedule"]["cron"]["expression"],
        "0 */6 * * *"
    );
    // The trigger keeps its id.
    assert!(m["monitor"]["triggers"][0]["query_level_trigger"]["id"].is_string());
    no_changes(&mut s, &mid).await;
    // A monitor runs whole ("Ejecutar ahora"), not from a step.
    let e = s.run_job_from(&mid, "Entradas").await.unwrap_err();
    assert!(e.to_string().contains("Ejecutar ahora"), "{e}");
    assert!(s.run_job_from(&tid, "Transform").await.is_err());
    let it = s
        .scheduler_items("job")
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id == mid)
        .unwrap();
    assert_eq!((it.can_edit, it.can_delete, it.actions), (None, None, None));

    // A per document monitor, with its type's inputs and triggers.
    let v = new_job(
        &mut s,
        json!({
            "type": "monitor",
            "name": "dbine_test_sched_doc",
            "monitor_type": "doc_level_monitor",
            "enabled": false,
            "m_schedule_kind": "period",
            "m_interval": 10,
            "m_interval_unit": "minutes",
            "inputs_doc": "[{\"doc_level_input\": {\"description\": \"\", \"indices\": [\"dbine_test_sched_src\"], \"queries\": [{\"id\": \"q1\", \"name\": \"q1\", \"query\": \"host:\\\"a\\\"\", \"tags\": []}]}}]",
            "triggers_doc": "[{\"document_level_trigger\": {\"name\": \"t\", \"severity\": \"1\", \"condition\": {\"script\": {\"source\": \"query[name=q1]\", \"lang\": \"painless\"}}, \"actions\": []}}]"
        }),
    )
    .await;
    let did = save(&mut s, None, &v).await;
    let (_, m) = http(
        "GET",
        &format!(
            "{base}/_plugins/_alerting/monitors/{}",
            did.trim_start_matches("monitor:")
        ),
        None,
    )
    .await;
    assert_eq!(m["monitor"]["monitor_type"], "doc_level_monitor", "{m}");
    assert_eq!(
        m["monitor"]["inputs"][0]["doc_level_input"]["indices"][0],
        "dbine_test_sched_src"
    );
    assert!(
        m["monitor"]["triggers"][0]["document_level_trigger"].is_object(),
        "{m}"
    );
    let form = s.scheduler_form("job", Some(&did)).await.unwrap();
    assert!(form.values["inputs_doc"]
        .as_str()
        .unwrap()
        .contains("doc_level_input"));
    no_changes(&mut s, &did).await;
    s.scheduler_delete("job", &did).await.unwrap();

    // ---- snapshot management
    let v = new_job(
        &mut s,
        json!({
            "type": "sm",
            "id": "dbine_test_sched_sm",
            "description": "diaria",
            "enabled": false,
            "creation_cron": "0 8 * * *",
            "creation_timezone": "UTC",
            "repository": "dbine_test_sched_repo",
            "indices": "dbine_test_sched_*",
            "deletion": true,
            "max_count": 5,
            "max_age": "7d"
        }),
    )
    .await;
    // The repository doesn't exist: refused before the server when the
    // cluster lists any; with none listed, the cluster takes the name.
    let has_repos = s
        .scheduler_form("job", None)
        .await
        .unwrap()
        .choices
        .get("repository")
        .is_some_and(|r| !r.is_empty());
    if !has_repos {
        let pid = save(&mut s, None, &v).await;
        assert_eq!(pid, "sm:dbine_test_sched_sm");
        assert!(items(&mut s).await.contains(&pid));
        let d = s.job_detail(&pid).await.unwrap();
        assert_eq!(d.job.schedule.as_deref(), Some("cron 0 8 * * * (UTC)"));
        assert!(!d.job.enabled);
        assert!(s
            .job_script(&pid)
            .await
            .unwrap()
            .contains("POST /_plugins/_sm/policies/dbine_test_sched_sm"));
        no_changes(&mut s, &pid).await;
        let v = edit(
            &mut s,
            &pid,
            json!({"max_count": 10, "enabled": true, "description": "otra"}),
        )
        .await;
        let sc = script(&mut s, Some(&pid), &v).await;
        assert!(sc.contains("if_seq_no=") && sc.contains("/_start"), "{sc}");
        save(&mut s, Some(&pid), &v).await;
        let (_, p) = http(
            "GET",
            &format!("{base}/_plugins/_sm/policies/dbine_test_sched_sm"),
            None,
        )
        .await;
        assert_eq!(p["sm_policy"]["deletion"]["condition"]["max_count"], 10);
        assert_eq!(p["sm_policy"]["enabled"], true);
        assert_eq!(p["sm_policy"]["description"], "otra");
        no_changes(&mut s, &pid).await;
        assert!(s.run_job(&pid).await.is_err());
        s.set_job_enabled(&pid, false).await.unwrap();
        assert!(!s.job_detail(&pid).await.unwrap().job.enabled);
        s.scheduler_delete("job", &pid).await.unwrap();
    } else {
        assert!(s.scheduler_script("job", None, &v).await.is_err());
    }

    // Logs.
    let logs = s.scheduler_logs().await.unwrap();
    assert!(logs.iter().any(|t| t.key == "alerts"), "{logs:?}");

    // Delete.
    for id in [tid.as_str(), mid.as_str()] {
        s.scheduler_delete("job", id).await.unwrap();
    }
    let ids = items(&mut s).await;
    assert!(!ids.contains(&tid) && !ids.contains(&mid), "{ids:?}");
}

/// ISM policies: create, change in place, a new id, delete.
async fn ism(s: &mut Box<dyn Session>, base: &str, p: &str) {
    let info = s.scheduler_info().await.unwrap();
    assert!(
        info.kinds.iter().any(|k| k.key == "ism" && k.can_create),
        "{info:?}"
    );
    let form = s.scheduler_form("ism", None).await.unwrap();
    let mut v = values_of(&form);
    set(
        &mut v,
        json!({
            "id": "dbine_test_sched_ism",
            "description": "borra a los 30 días",
            "default_state": "caliente",
            "ism_template": "[{\"index_patterns\": [\"dbine_test_sched_ism_*\"], \"priority\": 7}]"
        }),
    );
    let sc = s.scheduler_script("ism", None, &v).await.unwrap();
    assert!(
        sc.contains(&format!("PUT /{p}/_ism/policies/dbine_test_sched_ism")),
        "{sc}"
    );
    let id = s.scheduler_save("ism", None, &v).await.unwrap().unwrap();
    assert_eq!(id, "dbine_test_sched_ism");
    let list = s.scheduler_items("ism").await.unwrap();
    assert!(list.items.iter().any(|i| i.id == id), "{list:?}");
    let form = s.scheduler_form("ism", Some(&id)).await.unwrap();
    let v = values_of(&form);
    assert!(s
        .scheduler_script("ism", Some(&id), &v)
        .await
        .unwrap()
        .contains("no hay cambios"));
    let mut v2 = v.clone();
    set(&mut v2, json!({"description": "otra"}));
    let sc = s.scheduler_script("ism", Some(&id), &v2).await.unwrap();
    assert!(sc.contains("if_seq_no="), "{sc}");
    s.scheduler_save("ism", Some(&id), &v2).await.unwrap();
    let (_, got) = http("GET", &format!("{base}/{p}/_ism/policies/{id}"), None).await;
    let pol = got["policy"].get("policy").unwrap_or(&got["policy"]);
    assert_eq!(pol["description"], "otra", "{got}");
    // A new id the cluster refuses (an action ISM doesn't have): the old
    // policy is still there and the new one was never made.
    let mut bad = v2.clone();
    set(
        &mut bad,
        json!({
            "id": "dbine_test_sched_ism_bad",
            "states": "[{\"name\": \"caliente\", \"actions\": [{\"no_such_action\": {}}], \"transitions\": []}]"
        }),
    );
    let e = s.scheduler_save("ism", Some(&id), &bad).await.unwrap_err();
    assert!(!e.to_string().contains("Se aplicaron"), "{e}");
    let (st, _) = http("GET", &format!("{base}/{p}/_ism/policies/{id}"), None).await;
    assert_eq!(st, 200, "the old policy must survive a refused rename");
    let (st, _) = http(
        "GET",
        &format!("{base}/{p}/_ism/policies/dbine_test_sched_ism_bad"),
        None,
    )
    .await;
    assert_eq!(st, 404);
    // A new id with the template: created without it, the old one deleted,
    // the template added to the new one.
    let mut v3 = v2.clone();
    set(&mut v3, json!({"id": "dbine_test_sched_ism2"}));
    let sc = s.scheduler_script("ism", Some(&id), &v3).await.unwrap();
    let order: Vec<&str> = sc
        .lines()
        .filter(|l| l.starts_with("PUT ") || l.starts_with("DELETE "))
        .collect();
    assert_eq!(
        order,
        [
            format!("PUT /{p}/_ism/policies/dbine_test_sched_ism2"),
            format!("DELETE /{p}/_ism/policies/dbine_test_sched_ism"),
            format!("PUT /{p}/_ism/policies/dbine_test_sched_ism2"),
        ],
        "{sc}"
    );
    let id2 = s
        .scheduler_save("ism", Some(&id), &v3)
        .await
        .unwrap()
        .unwrap();
    let list = s.scheduler_items("ism").await.unwrap();
    assert!(list.items.iter().any(|i| i.id == id2) && !list.items.iter().any(|i| i.id == id));
    let (_, got) = http("GET", &format!("{base}/{p}/_ism/policies/{id2}"), None).await;
    let pol = got["policy"].get("policy").unwrap_or(&got["policy"]);
    assert!(
        pol["ism_template"]
            .to_string()
            .contains("dbine_test_sched_ism_*"),
        "{got}"
    );
    assert_eq!(pol["description"], "otra", "{got}");
    let sc = s.scheduler_delete_script("ism", &id2).await.unwrap();
    assert!(sc.contains("DELETE"), "{sc}");
    s.scheduler_delete("ism", &id2).await.unwrap();
    assert!(s.scheduler_form("ilm", None).await.is_err());
}

#[tokio::test]
#[ignore]
async fn opensearch_jobs() {
    let Some(base) = url("DBINE_TEST_OPENSEARCH_URL") else {
        return;
    };
    let b = base.clone();
    let body = async move {
        os_jobs(b.clone()).await;
        let mut s = open("opensearch", &b).await;
        ism(&mut s, &b, "_plugins").await;
    };
    with_cleanup(body, || os_cleanup(base, "_plugins")).await;
}

#[tokio::test]
#[ignore]
async fn opendistro_monitors() {
    let Some(base) = url("DBINE_TEST_OPENDISTRO_URL") else {
        return;
    };
    let b = base.clone();
    let body = async move {
        let mut s = open("opendistro", &b).await;
        source_index(&b, "dbine_test_sched_src").await;
        let info = s.scheduler_info().await.unwrap();
        assert!(info.kinds[0].can_create);
        let form = s.scheduler_form("job", None).await.unwrap();
        let opts = serde_json::to_value(&form.sections[0].fields[0].kind).unwrap();
        assert_eq!(opts["options"][0][0], "monitor", "{opts}");
        assert!(!form
            .sections
            .iter()
            .flat_map(|x| x.fields.iter())
            .any(|f| f.key == "monitor_type"));
        let v = new_job(
            &mut s,
            json!({
                "type": "monitor",
                "name": "dbine_test_sched_mon",
                "enabled": false,
                "inputs": "[{\"search\": {\"indices\": [\"dbine_test_sched_src\"], \"query\": {\"size\": 0, \"query\": {\"match_all\": {}}}}}]",
                "triggers": "[{\"name\": \"nunca\", \"severity\": \"1\", \"condition\": {\"script\": {\"source\": \"return false\", \"lang\": \"painless\"}}, \"actions\": []}]"
            }),
        )
        .await;
        let sc = script(&mut s, None, &v).await;
        assert!(sc.contains("POST /_opendistro/_alerting/monitors"), "{sc}");
        let mid = save(&mut s, None, &v).await;
        no_changes(&mut s, &mid).await;
        let v = edit(&mut s, &mid, json!({"m_interval": 3, "enabled": true})).await;
        save(&mut s, Some(&mid), &v).await;
        let raw = mid.trim_start_matches("monitor:");
        let (_, m) = http(
            "GET",
            &format!("{b}/_opendistro/_alerting/monitors/{raw}"),
            None,
        )
        .await;
        assert_eq!(m["monitor"]["schedule"]["period"]["interval"], 3);
        assert_eq!(m["monitor"]["enabled"], true);
        no_changes(&mut s, &mid).await;
        let logs = s.scheduler_logs().await.unwrap();
        assert!(logs.iter().any(|t| t.key == "alerts"), "{logs:?}");
        s.scheduler_delete("job", &mid).await.unwrap();
        assert!(!items(&mut s).await.contains(&mid));
        ism(&mut s, &b, "_opendistro").await;
    };
    with_cleanup(body, || os_cleanup(base, "_opendistro")).await;
}

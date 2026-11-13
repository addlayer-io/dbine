//! The scheduler designer against real servers (each test needs its
//! variable; skipped without it):
//! - `influxdb:2` in setup mode (org `dbine`, admin token `dbinetoken`):
//!   `DBINE_TEST_INFLUXDB_URL=http://localhost:25403`; tasks.
//! - `influxdb:1.8`: `DBINE_TEST_INFLUXDB1_URL=http://localhost:25404`;
//!   continuous queries. With `DBINE_TEST_INFLUXDB1_STATS_URL` (a server
//!   with `INFLUXDB_CONTINUOUS_QUERIES_QUERY_STATS_ENABLED=true` and
//!   `INFLUXDB_CONTINUOUS_QUERIES_RUN_INTERVAL=1s`), also clearing their
//!   history.
//! - `influxdb:3-core` (`--without-auth`, a plugin directory):
//!   `DBINE_TEST_INFLUXDB3_URL=http://localhost:25409`; processing engine
//!   triggers. InfluxDB has no API to delete a plugin file: with
//!   `DBINE_TEST_INFLUXDB3_CONTAINER=dbine-test-influxdb3` the test removes
//!   its own from the container's plugin directory.
//!
//! ```sh
//! DBINE_TEST_INFLUXDB_URL=http://localhost:25403 \
//! DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 \
//! DBINE_TEST_INFLUXDB3_URL=http://localhost:25409 \
//! DBINE_TEST_INFLUXDB3_CONTAINER=dbine-test-influxdb3 \
//!   cargo test -p dbine-driver-influxdb --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! Every object a test makes is named `dbine_test_sched_*` and is removed
//! at the end, whether the test passed or not.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{
    ConnectionConfig, Error, FormValues, ObjectForm, QueryOutcome, SchedulerItem, Session,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

async fn open(driver: &str, cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_influxdb::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    assert!(d.capabilities().scheduler);
    d.connect(cfg, None).await.expect("connect")
}

/// What the designer would send back for a form as it came: its values,
/// the fields' defaults where a value is missing, and its lists' rows.
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

fn set(v: &mut FormValues, k: &str, x: Value) {
    v.values.insert(k.into(), x);
}

async fn save(s: &mut Box<dyn Session>, id: Option<&str>, v: &FormValues) -> String {
    let script = s.scheduler_script("job", id, v).await.expect("script");
    eprintln!("--- {id:?}\n{script}");
    let r = s.scheduler_save("job", id, v).await;
    r.unwrap_or_else(|e| panic!("save: {e:?}\n{script}"))
        .expect("an id")
}

async fn item(s: &mut Box<dyn Session>, name: &str) -> Option<SchedulerItem> {
    s.scheduler_items("job")
        .await
        .expect("items")
        .items
        .into_iter()
        .find(|i| i.name == name)
}

/// The form of `id`, as the designer sends it back.
async fn current(s: &mut Box<dyn Session>, id: &str) -> FormValues {
    values_of(&s.scheduler_form("job", Some(id)).await.expect("form"))
}

/// Refused before anything reaches the server.
async fn refuses_ids(s: &mut Box<dyn Session>, bad: &[&str]) {
    for id in bad {
        assert!(s.scheduler_form("job", Some(id)).await.is_err(), "{id}");
        assert!(s.scheduler_delete_script("job", id).await.is_err(), "{id}");
        assert!(s.scheduler_delete("job", id).await.is_err(), "{id}");
        assert!(
            s.scheduler_save("job", Some(id), &FormValues::default())
                .await
                .is_err(),
            "{id}"
        );
    }
    assert!(s.scheduler_items("operator").await.is_err());
    assert!(s.scheduler_form("operator", None).await.is_err());
}

/// A read-only connection reads forms and scripts, and changes nothing.
async fn read_only_refuses(inner: Box<dyn Session>, id: &str) {
    let mut ro = ReadOnlySession::new(inner);
    let form = ro.scheduler_form("job", Some(id)).await.expect("form");
    let v = values_of(&form);
    assert!(ro.scheduler_script("job", Some(id), &v).await.is_ok());
    assert!(ro.scheduler_save("job", Some(id), &v).await.is_err());
    assert!(ro.scheduler_save("job", None, &v).await.is_err());
    assert!(ro.scheduler_delete("job", id).await.is_err());
    assert!(ro.clear_job_history(id).await.is_err());
}

// ---------------------------------------------------------------- 2.x

struct V2 {
    url: String,
    org: String,
    token: String,
}

impl V2 {
    fn env() -> Option<Self> {
        Some(V2 {
            url: std::env::var("DBINE_TEST_INFLUXDB_URL").ok()?,
            org: std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into()),
            token: std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into()),
        })
    }

    fn cfg(&self) -> ConnectionConfig {
        let mut cfg = ConnectionConfig {
            driver: "influxdb".into(),
            host: self.url.clone(),
            ..Default::default()
        };
        cfg.options.insert("org".into(), self.org.clone());
        cfg.options.insert("token".into(), self.token.clone());
        cfg
    }

    async fn call(&self, m: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut r = reqwest::Client::new()
            .request(m, format!("{}{path}", self.url))
            .header("Authorization", format!("Token {}", self.token));
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        assert!(status.is_success(), "{path}: {status} {text}");
        serde_json::from_str(&text).unwrap_or_default()
    }

    async fn org_id(&self) -> String {
        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/api/v2/orgs?org={}", self.org),
                None,
            )
            .await;
        v["orgs"][0]["id"].as_str().unwrap().to_string()
    }

    async fn task(&self, id: &str) -> Value {
        self.call(reqwest::Method::GET, &format!("/api/v2/tasks/{id}"), None)
            .await
    }

    async fn runs(&self, id: &str) -> usize {
        self.call(
            reqwest::Method::GET,
            &format!("/api/v2/tasks/{id}/runs"),
            None,
        )
        .await["runs"]
            .as_array()
            .map_or(0, Vec::len)
    }

    /// A threshold check named `name`: `(check id, its task's id)`.
    async fn check(&self, org_id: &str, name: &str) -> (String, String) {
        let flux = "from(bucket: \"test\")\n  |> range(start: v.timeRangeStart, stop: v.timeRangeStop)\n  |> filter(fn: (r) => r[\"_measurement\"] == \"m\")\n  |> filter(fn: (r) => r[\"_field\"] == \"v\")\n  |> aggregateWindow(every: 1h, fn: mean, createEmpty: false)\n  |> yield(name: \"mean\")";
        let v = self
            .call(
                reqwest::Method::POST,
                "/api/v2/checks",
                Some(json!({
                    "name": name, "orgID": org_id, "type": "threshold", "status": "inactive",
                    "every": "1h", "offset": "0s", "statusMessageTemplate": "x", "tags": [],
                    "query": {"text": flux, "editMode": "advanced", "name": "",
                              "builderConfig": {"buckets": [], "tags": [], "functions": []}},
                    "thresholds": [{"type": "greater", "value": 10, "level": "CRIT", "allValues": false}],
                })),
            )
            .await;
        (
            v["id"].as_str().unwrap().to_string(),
            v["taskID"].as_str().unwrap().to_string(),
        )
    }

    /// The `dbine_test_sched_*` checks.
    async fn checks(&self) -> Vec<String> {
        let org_id = self.org_id().await;
        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/api/v2/checks?orgID={org_id}"),
                None,
            )
            .await;
        v["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| {
                c["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("dbine_test_sched_"))
            })
            .filter_map(|c| c["id"].as_str().map(String::from))
            .collect()
    }

    /// Deletes every `dbine_test_sched_*` check (with its task), task and
    /// label.
    async fn clean(&self) {
        for id in self.checks().await {
            self.call(
                reqwest::Method::DELETE,
                &format!("/api/v2/checks/{id}"),
                None,
            )
            .await;
        }
        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/api/v2/tasks?org={}&limit=500", self.org),
                None,
            )
            .await;
        for t in v["tasks"].as_array().into_iter().flatten() {
            if t["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("dbine_test_sched_"))
            {
                let id = t["id"].as_str().unwrap();
                self.call(
                    reqwest::Method::DELETE,
                    &format!("/api/v2/tasks/{id}"),
                    None,
                )
                .await;
            }
        }
        let v = self
            .call(reqwest::Method::GET, "/api/v2/labels", None)
            .await;
        for l in v["labels"].as_array().into_iter().flatten() {
            if l["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("dbine_test_sched_"))
            {
                let id = l["id"].as_str().unwrap();
                self.call(
                    reqwest::Method::DELETE,
                    &format!("/api/v2/labels/{id}"),
                    None,
                )
                .await;
            }
        }
    }

    async fn label(&self, org_id: &str, name: &str) -> String {
        let v = self
            .call(
                reqwest::Method::POST,
                "/api/v2/labels",
                Some(json!({ "orgID": org_id, "name": name })),
            )
            .await;
        v["label"]["id"].as_str().unwrap().to_string()
    }

    async fn left(&self) -> usize {
        let t = self
            .call(
                reqwest::Method::GET,
                &format!("/api/v2/tasks?org={}&limit=500", self.org),
                None,
            )
            .await;
        let l = self
            .call(reqwest::Method::GET, "/api/v2/labels", None)
            .await;
        [t["tasks"].clone(), l["labels"].clone()]
            .iter()
            .flat_map(|a| a.as_array().cloned().unwrap_or_default())
            .filter(|x| {
                x["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("dbine_test_sched_"))
            })
            .count()
            + self.checks().await.len()
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb2_tasks_designed() {
    let Some(v2) = V2::env() else {
        eprintln!("DBINE_TEST_INFLUXDB_URL not set; skipping");
        return;
    };
    v2.clean().await;
    let body = tokio::spawn(async move {
        let v2 = V2::env().unwrap();
        influxdb2_body(&v2).await
    })
    .await;
    v2.clean().await;
    assert_eq!(v2.left().await, 0, "objects left behind");
    body.unwrap();
}

async fn influxdb2_body(v2: &V2) {
    let org_id = v2.org_id().await;
    let prod = v2.label(&org_id, "dbine_test_sched_prod").await;
    let etl = v2.label(&org_id, "dbine_test_sched_etl").await;
    let mut s = open("influxdb", &v2.cfg()).await;

    let info = s.scheduler_info().await.expect("info");
    assert_eq!(info.kinds.len(), 1);
    assert_eq!(info.kinds[0].key, "job");
    assert_eq!(info.kinds[0].label, "Tareas");
    assert!(info.kinds[0].can_create && info.kinds[0].can_edit && info.kinds[0].can_delete);
    assert!(!info.logs);
    // Runs live in `_tasks` (a history to clear); "run now" is the job's.
    let k = &info.kinds[0];
    assert!(
        k.clear_history && !k.run_from_step && k.actions.is_empty(),
        "{k:?}"
    );

    // A new task, with an import, a label and a description.
    let form = s.scheduler_form("job", None).await.expect("form");
    assert!(form.choices["label"].iter().any(|(id, _)| *id == prod));
    let mut v = values_of(&form);
    set(&mut v, "name", json!("dbine_test_sched_task"));
    set(&mut v, "description", json!("DBine's test"));
    set(&mut v, "every", json!("1h"));
    set(&mut v, "offset", json!("5m"));
    set(&mut v, "labels", json!([prod]));
    set(
        &mut v,
        "flux",
        json!("import \"array\"\n\narray.from(rows: [{_value: 1}])\n  |> yield()"),
    );
    let id = save(&mut s, None, &v).await;
    let t = v2.task(&id).await;
    assert_eq!(t["name"], "dbine_test_sched_task");
    assert_eq!(t["every"], "1h");
    assert_eq!(t["offset"], "5m");
    assert_eq!(t["status"], "active");
    assert_eq!(t["description"], "DBine's test");
    assert_eq!(t["labels"][0]["id"], json!(prod));
    assert!(t["flux"]
        .as_str()
        .unwrap()
        .starts_with("import \"array\"\n\noption task = {name: \"dbine_test_sched_task\", every: 1h, offset: 5m}"));
    // The tree's row and the jobs' row are the same object.
    let it = item(&mut s, "dbine_test_sched_task").await.unwrap();
    assert_eq!(it.id, id);
    assert_eq!(it.enabled, Some(true));
    // A task of its own (the list carries its Flux): editable.
    assert_eq!((it.can_edit, it.can_delete), (None, None), "{it:?}");
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));

    // A check's task goes with its check (deleting it alone leaves the
    // check broken: InfluxDB then refuses to read, change or delete it).
    // InfluxDB 2.x leaves it out of the task list, so neither the tree nor
    // the jobs offer it; a server that lists it with its `type` gets it
    // marked neither editable nor deletable (unit tests).
    let (check, check_task) = v2.check(&org_id, "dbine_test_sched_check").await;
    let items = s.scheduler_items("job").await.expect("items").items;
    assert!(items.iter().all(|i| i.id != check_task), "{items:?}");
    assert!(s
        .jobs()
        .await
        .unwrap()
        .jobs
        .iter()
        .all(|j| j.id != check_task));
    v2.call(
        reqwest::Method::DELETE,
        &format!("/api/v2/checks/{check}"),
        None,
    )
    .await;

    // The form reads it back; unchanged, there's nothing to do.
    let form = s.scheduler_form("job", Some(&id)).await.expect("form");
    assert_eq!(
        form.values["flux"],
        "import \"array\"\n\narray.from(rows: [{_value: 1}])\n  |> yield()"
    );
    assert_eq!(form.values["owner"], "admin");
    let v = values_of(&form);
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(script.contains("no hay cambios"), "{script}");
    assert_eq!(
        s.scheduler_save("job", Some(&id), &v).await.unwrap(),
        Some(id.clone())
    );

    // Each part, in place.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "every", json!("2h"));
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(
        script.contains("// PATCH /api/v2/tasks/") && script.contains("\"every\": \"2h\""),
        "{script}"
    );
    assert!(!script.contains("\"flux\""), "{script}");
    save(&mut s, Some(&id), &v).await;
    assert_eq!(v2.task(&id).await["every"], "2h");

    let mut v = current(&mut s, &id).await;
    set(&mut v, "mode", json!("cron"));
    set(&mut v, "cron", json!("0 3 * * *"));
    save(&mut s, Some(&id), &v).await;
    let t = v2.task(&id).await;
    assert_eq!(t["cron"], "0 3 * * *");
    assert!(t["every"].is_null());

    let mut v = current(&mut s, &id).await;
    set(&mut v, "offset", json!(""));
    set(&mut v, "description", json!(""));
    set(&mut v, "name", json!("dbine_test_sched_task 2"));
    set(&mut v, "enabled", json!(false));
    set(&mut v, "labels", json!([etl]));
    save(&mut s, Some(&id), &v).await;
    let t = v2.task(&id).await;
    assert!(t["offset"].is_null(), "{t}");
    assert!(t["description"].is_null() || t["description"] == "", "{t}");
    assert_eq!(t["name"], "dbine_test_sched_task 2");
    assert_eq!(t["status"], "inactive");
    let labels: Vec<&str> = t["labels"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l["id"].as_str())
        .collect();
    assert_eq!(labels, [etl.as_str()]);

    let mut v = current(&mut s, &id).await;
    set(&mut v, "mode", json!("every"));
    set(&mut v, "every", json!("30m"));
    set(&mut v, "enabled", json!(true));
    set(
        &mut v,
        "flux",
        json!("import \"array\"\narray.from(rows: [{_value: 2}])"),
    );
    save(&mut s, Some(&id), &v).await;
    let t = v2.task(&id).await;
    assert_eq!(t["every"], "30m");
    assert_eq!(t["status"], "active");
    assert!(t["flux"].as_str().unwrap().contains("_value: 2"), "{t}");

    // Another option the task had stays when DBine writes the Flux.
    let made = v2
        .call(
            reqwest::Method::POST,
            "/api/v2/tasks",
            Some(json!({
                "org": v2.org,
                "flux": "import \"array\"\noption task = {name: \"dbine_test_sched_extra\", every: 1h, offset: 1m, concurrency: 1, foo: 1}\narray.from(rows: [{_value: 1}])",
            })),
        )
        .await;
    let extra = made["id"].as_str().unwrap().to_string();
    let form = s.scheduler_form("job", Some(&extra)).await.unwrap();
    assert_eq!(form.values["concurrency"], 1);
    assert_eq!(form.values["retry"], Value::Null);
    assert_eq!(form.values["other_options"], "foo: 1");
    let mut v = current(&mut s, &extra).await;
    set(&mut v, "offset", json!(""));
    save(&mut s, Some(&extra), &v).await;
    let t = v2.task(&extra).await;
    assert!(t["offset"].is_null());
    assert!(
        t["flux"]
            .as_str()
            .unwrap()
            .contains("{name: \"dbine_test_sched_extra\", every: 1h, concurrency: 1, foo: 1}"),
        "{t}"
    );
    // Runs at once and retries: written in the Flux, the rest kept.
    let mut v = current(&mut s, &extra).await;
    set(&mut v, "concurrency", json!(3));
    set(&mut v, "retry", json!(2));
    save(&mut s, Some(&extra), &v).await;
    let t = v2.task(&extra).await;
    assert!(
        t["flux"].as_str().unwrap().contains(
            "{name: \"dbine_test_sched_extra\", every: 1h, concurrency: 3, retry: 2, foo: 1}"
        ),
        "{t}"
    );
    let form = s.scheduler_form("job", Some(&extra)).await.unwrap();
    assert_eq!(
        (&form.values["concurrency"], &form.values["retry"]),
        (&json!(3), &json!(2))
    );
    let mut v = current(&mut s, &extra).await;
    set(&mut v, "retry", json!(11));
    assert!(s.scheduler_save("job", Some(&extra), &v).await.is_err());

    // Refused before anything changes.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "labels", json!(["0000000000000000"]));
    assert!(s.scheduler_save("job", Some(&id), &v).await.is_err());
    set(&mut v, "labels", json!([]));
    set(&mut v, "every", json!("1h; drop"));
    assert!(s.scheduler_save("job", Some(&id), &v).await.is_err());
    // Rejected by the server: nothing changes.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "flux", json!("from(bucket: "));
    assert!(s.scheduler_save("job", Some(&id), &v).await.is_err());
    assert!(v2.task(&id).await["flux"]
        .as_str()
        .unwrap()
        .contains("_value: 2"));

    // Run now, then clear its history (once the run is over: a run going
    // on is written to `_tasks` when it ends).
    s.run_job(&id).await.unwrap();
    let mut done = false;
    for _ in 0..80 {
        let runs = v2
            .call(
                reqwest::Method::GET,
                &format!("/api/v2/tasks/{id}/runs"),
                None,
            )
            .await;
        let runs = runs["runs"].as_array().cloned().unwrap_or_default();
        if !runs.is_empty()
            && runs
                .iter()
                .all(|r| !matches!(r["status"].as_str(), Some("started" | "scheduled")))
        {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(done, "the run");
    tokio::time::sleep(Duration::from_secs(1)).await;
    s.clear_job_history(&id).await.unwrap();
    assert_eq!(v2.runs(&id).await, 0);
    assert!(matches!(
        s.run_job_from(&id, "x").await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        s.scheduler_logs().await,
        Err(Error::Unsupported(_))
    ));

    read_only_refuses(open("influxdb", &v2.cfg()).await, &id).await;

    // Delete.
    let script = s.scheduler_delete_script("job", &id).await.unwrap();
    assert!(
        script.contains(&format!("// DELETE /api/v2/tasks/{id}")),
        "{script}"
    );
    s.scheduler_delete("job", &id).await.unwrap();
    assert!(item(&mut s, "dbine_test_sched_task 2").await.is_none());
    s.scheduler_delete("job", &extra).await.unwrap();

    refuses_ids(
        &mut s,
        &[
            "x' OR 1=1 --",
            "../../orgs",
            "0123456789abcdef/runs",
            "0000000000000000",
        ],
    )
    .await;
}

// ---------------------------------------------------------------- 1.x

async fn v1_run(url: &str, q: &str) {
    let r = reqwest::Client::new()
        .post(format!("{url}/query"))
        .form(&[("q", q)])
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{q}");
}

async fn v1_cqs(url: &str) -> Vec<(String, String, String)> {
    let v: Value = reqwest::Client::new()
        .get(format!("{url}/query"))
        .query(&[("q", "SHOW CONTINUOUS QUERIES")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut out = Vec::new();
    for s in v["results"][0]["series"].as_array().into_iter().flatten() {
        for r in s["values"].as_array().into_iter().flatten() {
            out.push((
                s["name"].as_str().unwrap().to_string(),
                r[0].as_str().unwrap().to_string(),
                r[1].as_str().unwrap().to_string(),
            ));
        }
    }
    out
}

async fn v1_clean(url: &str) {
    for db in ["dbine_test_sched_db", "dbine_test_sched_db2"] {
        v1_run(url, &format!("DROP DATABASE \"{db}\"")).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb1_continuous_queries_designed() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_URL not set; skipping");
        return;
    };
    v1_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move { v1_body(&u).await }).await;
    v1_clean(&url).await;
    let left = v1_cqs(&url)
        .await
        .iter()
        .filter(|(db, _, _)| db.starts_with("dbine_test_sched_"))
        .count();
    assert_eq!(left, 0, "objects left behind");
    body.unwrap();
}

async fn v1_body(url: &str) {
    v1_run(url, "CREATE DATABASE dbine_test_sched_db").await;
    v1_run(url, "CREATE DATABASE dbine_test_sched_db2").await;
    let cfg = ConnectionConfig {
        driver: "influxdb1".into(),
        host: url.into(),
        ..Default::default()
    };
    let mut s = open("influxdb1", &cfg).await;
    let info = s.scheduler_info().await.expect("info");
    assert_eq!(info.kinds.len(), 1);
    assert_eq!(info.kinds[0].label, "Consultas continuas");
    assert!(info.kinds[0].can_create && info.kinds[0].can_edit && info.kinds[0].can_delete);
    assert!(info.notes[0].contains("ALTER"));
    // Without query-stats-enabled there's no history to clear.
    let k = &info.kinds[0];
    assert!(
        !k.clear_history && !k.run_from_step && k.actions.is_empty(),
        "{k:?}"
    );

    // New.
    let form = s.scheduler_form("job", None).await.expect("form");
    assert!(form.choices["database"]
        .iter()
        .any(|(d, _)| d == "dbine_test_sched_db"));
    let mut v = values_of(&form);
    set(&mut v, "database", json!("dbine_test_sched_db"));
    set(&mut v, "name", json!("dbine_test_sched_cq"));
    set(&mut v, "resample_every", json!("30m"));
    set(&mut v, "resample_for", json!("2h"));
    set(
        &mut v,
        "query",
        json!("SELECT mean(\"v\") AS \"m\" INTO \"cpu_1h\" FROM \"cpu\" WHERE \"host\" = 'a;b' GROUP BY time(1h), *"),
    );
    let id = save(&mut s, None, &v).await;
    assert_eq!(id, "\"dbine_test_sched_db\".\"dbine_test_sched_cq\"");
    let (_, _, q) = v1_cqs(url)
        .await
        .into_iter()
        .find(|(_, n, _)| n == "dbine_test_sched_cq")
        .unwrap();
    assert!(q.contains("RESAMPLE EVERY 30m FOR 2h"), "{q}");
    assert!(q.contains("host = 'a;b'"), "{q}");
    let it = item(&mut s, "dbine_test_sched_cq").await.unwrap();
    assert_eq!(it.id, id);
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));

    // Read back; unchanged, nothing to do.
    let form = s.scheduler_form("job", Some(&id)).await.expect("form");
    assert_eq!(form.values["resample_every"], "30m");
    assert_eq!(form.values["group_by"], "1h");
    let v = values_of(&form);
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(script.contains("no hay cambios"), "{script}");

    // A change: dropped and created again.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "resample_every", json!("15m"));
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(
        script.contains("DROP CONTINUOUS QUERY") && script.contains("RESAMPLE EVERY 15m"),
        "{script}"
    );
    save(&mut s, Some(&id), &v).await;
    let q = v1_cqs(url).await;
    assert!(
        q.iter()
            .any(|(_, n, q)| n == "dbine_test_sched_cq" && q.contains("RESAMPLE EVERY 15m FOR 2h")),
        "{q:?}"
    );

    // The server refuses the new one (FOR under the GROUP BY interval):
    // the old one is back.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "resample_for", json!("30m"));
    let e = s.scheduler_save("job", Some(&id), &v).await.unwrap_err();
    assert!(e.to_string().contains("Se volvió a crear"), "{e}");
    let q = v1_cqs(url).await;
    assert!(
        q.iter()
            .any(|(_, n, q)| n == "dbine_test_sched_cq" && q.contains("RESAMPLE EVERY 15m FOR 2h")),
        "{q:?}"
    );

    // Another name and database: a new id, the old one gone.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "database", json!("dbine_test_sched_db2"));
    set(&mut v, "name", json!("dbine_test_sched_cq 2"));
    set(&mut v, "resample_every", json!(""));
    set(&mut v, "resample_for", json!(""));
    set(
        &mut v,
        "query",
        json!("SELECT count(\"v\") INTO \"cpu_5m\" FROM \"cpu\" GROUP BY time(5m)"),
    );
    let id2 = save(&mut s, Some(&id), &v).await;
    assert_eq!(id2, "\"dbine_test_sched_db2\".\"dbine_test_sched_cq 2\"");
    let q = v1_cqs(url).await;
    assert!(!q.iter().any(|(_, n, _)| n == "dbine_test_sched_cq"));
    assert!(q.iter().any(|(d, n, q)| d == "dbine_test_sched_db2"
        && n == "dbine_test_sched_cq 2"
        && !q.contains("RESAMPLE")));

    // Refused before it reaches the server.
    let mut v = current(&mut s, &id2).await;
    set(
        &mut v,
        "query",
        json!(
            "SELECT count(v) INTO x FROM cpu GROUP BY time(5m); DROP DATABASE dbine_test_sched_db"
        ),
    );
    assert!(s.scheduler_script("job", Some(&id2), &v).await.is_err());
    set(
        &mut v,
        "query",
        json!("SELECT count(v) INTO x FROM cpu GROUP BY time(5m)"),
    );
    set(&mut v, "database", json!("dbine_test_sched_nope"));
    assert!(s.scheduler_save("job", Some(&id2), &v).await.is_err());

    // No history without query stats; no steps.
    assert!(matches!(
        s.clear_job_history(&id2).await,
        Err(Error::Unsupported(_))
    ));
    assert!(s.run_job_from(&id2, "x").await.is_err());
    read_only_refuses(open("influxdb1", &cfg).await, &id2).await;

    // Delete.
    let script = s.scheduler_delete_script("job", &id2).await.unwrap();
    assert!(
        script.contains(
            "DROP CONTINUOUS QUERY \"dbine_test_sched_cq 2\" ON \"dbine_test_sched_db2\";"
        ),
        "{script}"
    );
    s.scheduler_delete("job", &id2).await.unwrap();
    assert!(item(&mut s, "dbine_test_sched_cq 2").await.is_none());

    refuses_ids(
        &mut s,
        &[
            "x' OR 1=1 --",
            "\"dbine_test_sched_db\".\"x\"; DROP DATABASE dbine_test_sched_db",
            "dbine_test_sched_db.dbine_test_sched_cq",
            "\"dbine_test_sched_db\".\"nope\"",
        ],
    )
    .await;
}

/// Clearing a continuous query's runs from `_internal` (a server with
/// query stats).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb1_continuous_query_history_cleared() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_STATS_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_STATS_URL not set; skipping");
        return;
    };
    v1_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move {
        let url = u;
        v1_run(&url, "CREATE DATABASE dbine_test_sched_db").await;
        let cfg = ConnectionConfig {
            driver: "influxdb1".into(),
            host: url.clone(),
            ..Default::default()
        };
        let mut s = open("influxdb1", &cfg).await;
        let mut v = values_of(&s.scheduler_form("job", None).await.unwrap());
        set(&mut v, "database", json!("dbine_test_sched_db"));
        set(&mut v, "name", json!("dbine_test_sched_cq"));
        set(&mut v, "resample_every", json!("1s"));
        set(&mut v, "resample_for", json!("2s"));
        set(
            &mut v,
            "query",
            json!("SELECT count(v) INTO cpu_1s FROM cpu GROUP BY time(1s), *"),
        );
        let id = save(&mut s, None, &v).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let lines: String = (0..5)
            .map(|i| format!("cpu,host=a v={i} {}\n", now - i))
            .collect();
        reqwest::Client::new()
            .post(format!("{url}/write?db=dbine_test_sched_db&precision=s"))
            .body(lines)
            .send()
            .await
            .unwrap();
        let mut runs = Vec::new();
        for _ in 0..40 {
            runs = s.job_runs(&id, 5).await.unwrap_or_default();
            if !runs.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(!runs.is_empty(), "runs recorded");
        // Let a few more runs in, then clear: the query keeps running once
        // a second, so at most a run or two is recorded again at once.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let count = "SELECT count(\"durationNs\") FROM \"_internal\".\"monitor\".\"cq_query\" WHERE \"cq\" = 'dbine_test_sched_cq'";
        let mut out = QueryOutcome::default();
        s.execute(count, 10, &mut out).await.unwrap();
        let total = |out: &QueryOutcome| {
            out.results
                .iter()
                .flat_map(|r| r.rows.iter())
                .filter_map(|r| r.get(1).and_then(Value::as_i64))
                .sum::<i64>()
        };
        let before = total(&out);
        assert!(before >= 3, "{before}");
        // With query stats on (runs recorded), the kind says there's a
        // history to clear.
        let info = s.scheduler_info().await.unwrap();
        assert!(info.kinds[0].clear_history, "{info:?}");
        s.clear_job_history(&id).await.unwrap();
        let mut out = QueryOutcome::default();
        s.execute(count, 10, &mut out).await.unwrap();
        let after = total(&out);
        assert!(after <= 2, "{before} → {after}");
        s.scheduler_delete("job", &id).await.unwrap();
    })
    .await;
    v1_clean(&url).await;
    body.unwrap();
}

// ---------------------------------------------------------------- 3.x

const PLUGIN: &str = "dbine_test_sched_plugin.py";
const TICK: &str = "def process_scheduled_call(influxdb3_local, call_time, args=None):\n    influxdb3_local.info(\"dbine tick \" + str((args or {}).get(\"k\")))\n";
/// Logs `dbine tock <nonce>`.
fn tock(nonce: &str) -> String {
    format!("def process_scheduled_call(influxdb3_local, call_time, args=None):\n    influxdb3_local.info(\"dbine tock {nonce}\")\n")
}

async fn v3_clean(url: &str) {
    for db in ["dbine_test_sched_db", "dbine_test_sched_db2"] {
        let _ = reqwest::Client::new()
            .delete(format!("{url}/api/v3/configure/database"))
            .query(&[("db", db)])
            .send()
            .await;
    }
    // InfluxDB has no API to delete a plugin file.
    if let Ok(c) = std::env::var("DBINE_TEST_INFLUXDB3_CONTAINER") {
        if c.starts_with("dbine-test-") {
            let out = std::process::Command::new("docker")
                .args([
                    "exec",
                    &c,
                    "sh",
                    "-c",
                    "rm -f \"${INFLUXDB3_PLUGIN_DIR:-/plugins}\"/dbine_test_sched_*",
                ])
                .output();
            if let Err(e) = out {
                eprintln!("plugin files not removed: {e}");
            }
        }
    }
}

async fn v3_sql(url: &str, db: &str, q: &str) -> Vec<Value> {
    let body = reqwest::Client::new()
        .post(format!("{url}/api/v3/query_sql"))
        .json(&json!({ "db": db, "q": q, "format": "jsonl" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

async fn triggers(url: &str, db: &str) -> Vec<Value> {
    v3_sql(url, db, "SELECT * FROM system.processing_engine_triggers").await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb3_triggers_designed() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB3_URL not set; skipping");
        return;
    };
    v3_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move { v3_body(&u).await }).await;
    v3_clean(&url).await;
    let dbs: Value = reqwest::Client::new()
        .get(format!("{url}/api/v3/configure/database?format=json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !dbs.to_string().contains("dbine_test_sched_"),
        "objects left behind: {dbs}"
    );
    body.unwrap();
}

/// Waits for a log line of the trigger with `text`.
async fn logged(s: &mut Box<dyn Session>, trigger: &str, text: &str) {
    for _ in 0..60 {
        let logs = s.scheduler_logs().await.expect("logs");
        if logs[0]
            .rows
            .iter()
            .any(|r| r[2] == json!(trigger) && r[4].as_str().is_some_and(|m| m.contains(text)))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("{trigger} never logged {text}");
}

async fn v3_body(url: &str) {
    // The log outlives a deleted trigger: this run's lines carry their own
    // mark.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string();
    for db in ["dbine_test_sched_db", "dbine_test_sched_db2"] {
        let r = reqwest::Client::new()
            .post(format!("{url}/api/v3/configure/database"))
            .json(&json!({ "db": db }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }
    let cfg = ConnectionConfig {
        driver: "influxdb3".into(),
        host: url.into(),
        ..Default::default()
    };
    let mut s = open("influxdb3", &cfg).await;
    let info = s.scheduler_info().await.expect("info");
    assert_eq!(info.kinds[0].label, "Triggers");
    assert!(info.logs);
    // No API fires a trigger nor clears the engine's log; no steps.
    let k = &info.kinds[0];
    assert!(
        !k.clear_history && !k.run_from_step && k.actions.is_empty(),
        "{k:?}"
    );

    // New, with its plugin's code uploaded.
    let form = s.scheduler_form("job", None).await.expect("form");
    assert!(form.choices["database"]
        .iter()
        .any(|(d, _)| d == "dbine_test_sched_db"));
    let mut v = values_of(&form);
    set(&mut v, "database", json!("dbine_test_sched_db"));
    set(&mut v, "name", json!("dbine_test_sched_tick"));
    set(&mut v, "plugin", json!(PLUGIN));
    set(&mut v, "plugin_code", json!(TICK));
    set(&mut v, "fires", json!("every"));
    set(&mut v, "every", json!("1s"));
    v.lists.insert(
        "arguments".into(),
        vec![BTreeMap::from([
            ("key".to_string(), json!("k")),
            ("value".to_string(), json!(format!("v,w {nonce}"))),
        ])],
    );
    let id = save(&mut s, None, &v).await;
    assert_eq!(id, "\"dbine_test_sched_db\".\"dbine_test_sched_tick\"");
    let t = triggers(url, "dbine_test_sched_db").await;
    assert_eq!(t[0]["trigger_name"], "dbine_test_sched_tick");
    assert_eq!(t[0]["disabled"], false);
    logged(
        &mut s,
        "dbine_test_sched_tick",
        &format!("dbine tick v,w {nonce}"),
    )
    .await;
    let it = item(&mut s, "dbine_test_sched_tick").await.unwrap();
    assert_eq!(it.id, id);
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));

    // Read back; unchanged, nothing to do.
    let form = s.scheduler_form("job", Some(&id)).await.expect("form");
    assert_eq!(form.values["every"], "1s");
    assert_eq!(form.values["plugin"], PLUGIN);
    assert!(form.values["plugin_path"]
        .as_str()
        .is_some_and(|p| p.ends_with(PLUGIN)));
    assert_eq!(
        form.sections[3].lists[0].rows[0]["value"],
        json!(format!("v,w {nonce}"))
    );
    let script = s
        .scheduler_script("job", Some(&id), &values_of(&form))
        .await
        .unwrap();
    assert!(script.contains("no hay cambios"), "{script}");

    // In place: disabled, its code, enabled again.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "enabled", json!(false));
    set(&mut v, "plugin_code", json!(tock(&nonce)));
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(
        script.contains("-- PUT /api/v3/plugins/files?db=dbine_test_sched_db"),
        "{script}"
    );
    assert!(
        script.contains("processing_engine_trigger/disable?"),
        "{script}"
    );
    assert!(!script.contains("DELETE"), "{script}");
    save(&mut s, Some(&id), &v).await;
    assert_eq!(
        triggers(url, "dbine_test_sched_db").await[0]["disabled"],
        true
    );
    let mut v = current(&mut s, &id).await;
    set(&mut v, "enabled", json!(true));
    save(&mut s, Some(&id), &v).await;
    logged(
        &mut s,
        "dbine_test_sched_tick",
        &format!("dbine tock {nonce}"),
    )
    .await;

    // Again: a new schedule, arguments and error behavior.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "fires", json!("cron"));
    set(&mut v, "cron", json!("0 0 3 * * *"));
    set(&mut v, "error_behavior", json!("retry"));
    v.lists.insert(
        "arguments".into(),
        vec![BTreeMap::from([
            ("key".to_string(), json!("x")),
            ("value".to_string(), json!("1")),
        ])],
    );
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(script.contains("se borra y se vuelve a crear"), "{script}");
    save(&mut s, Some(&id), &v).await;
    let t = triggers(url, "dbine_test_sched_db").await;
    assert_eq!(t.len(), 1);
    assert_eq!(t[0]["error_behavior"], "retry");
    assert!(t[0]["trigger_specification"]
        .as_str()
        .unwrap()
        .contains("0 0 3 * * *"));
    let form = s.scheduler_form("job", Some(&id)).await.unwrap();
    assert_eq!(form.values["fires"], "cron");
    assert_eq!(form.sections[3].lists[0].rows[0]["key"], "x");

    // The server refuses the new one (no such plugin): the old one is back.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "plugin", json!("dbine_test_sched_missing.py"));
    let e = s.scheduler_save("job", Some(&id), &v).await.unwrap_err();
    assert!(e.to_string().contains("Se volvió a crear"), "{e}");
    let t = triggers(url, "dbine_test_sched_db").await;
    assert_eq!(t.len(), 1);
    assert_eq!(t[0]["plugin_filename"], PLUGIN);
    assert_eq!(t[0]["error_behavior"], "retry");

    // Another name and database: the new one first, the old one gone.
    let mut v = current(&mut s, &id).await;
    set(&mut v, "database", json!("dbine_test_sched_db2"));
    set(&mut v, "name", json!("dbine_test_sched_tock"));
    set(&mut v, "fires", json!("request"));
    set(&mut v, "path", json!("dbine_test_sched_hook"));
    set(&mut v, "run_async", json!(true));
    v.lists.insert("arguments".into(), Vec::new());
    let id2 = save(&mut s, Some(&id), &v).await;
    assert_eq!(id2, "\"dbine_test_sched_db2\".\"dbine_test_sched_tock\"");
    assert!(triggers(url, "dbine_test_sched_db").await.is_empty());
    let t = triggers(url, "dbine_test_sched_db2").await;
    assert_eq!(t[0]["trigger_name"], "dbine_test_sched_tock");
    assert!(t[0]["trigger_specification"]
        .as_str()
        .unwrap()
        .contains("dbine_test_sched_hook"));
    let args = v3_sql(
        url,
        "dbine_test_sched_db2",
        "SELECT * FROM system.processing_engine_trigger_arguments",
    )
    .await;
    assert!(args.is_empty(), "{args:?}");

    // Refused before it reaches the server.
    let mut v = current(&mut s, &id2).await;
    set(&mut v, "plugin", json!("../../etc/passwd"));
    assert!(s.scheduler_script("job", Some(&id2), &v).await.is_err());
    set(&mut v, "plugin", json!(PLUGIN));
    set(&mut v, "fires", json!("cron"));
    set(&mut v, "cron", json!("0 3 * * *"));
    assert!(s.scheduler_save("job", Some(&id2), &v).await.is_err());
    assert_eq!(triggers(url, "dbine_test_sched_db2").await.len(), 1);

    assert!(matches!(
        s.clear_job_history(&id2).await,
        Err(Error::Unsupported(_))
    ));
    assert!(s.run_job_from(&id2, "x").await.is_err());
    read_only_refuses(open("influxdb3", &cfg).await, &id2).await;

    // Delete (it's enabled: forced).
    let script = s.scheduler_delete_script("job", &id2).await.unwrap();
    assert!(script.contains("\"force\": true"), "{script}");
    s.scheduler_delete("job", &id2).await.unwrap();
    assert!(triggers(url, "dbine_test_sched_db2").await.is_empty());

    refuses_ids(
        &mut s,
        &[
            "x' OR 1=1 --",
            "\"dbine_test_sched_db\".\"dbine_test_sched_tick\"&trigger_name=x",
            "dbine_test_sched_db.dbine_test_sched_tick",
            "\"dbine_test_sched_db\".\"x' OR '1'='1\"",
        ],
    )
    .await;
}

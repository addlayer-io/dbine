//! Server jobs against real servers (each test needs its variable):
//! - `influxdb:2` in setup mode (org `dbine`, bucket `test`, admin token
//!   `dbinetoken`): `DBINE_TEST_INFLUXDB_URL=http://localhost:25403`; tasks.
//! - `influxdb:1.8`: `DBINE_TEST_INFLUXDB1_URL=http://localhost:25404`;
//!   continuous queries, without run history.
//! - `influxdb:1.8` with `INFLUXDB_CONTINUOUS_QUERIES_QUERY_STATS_ENABLED=true`
//!   and `INFLUXDB_CONTINUOUS_QUERIES_RUN_INTERVAL=1s`:
//!   `DBINE_TEST_INFLUXDB1_STATS_URL=http://localhost:25414`; the runs.
//! - `influxdb:3-core` (`serve --without-auth --plugin-dir /plugins`):
//!   `DBINE_TEST_INFLUXDB3_URL=http://localhost:25409`; processing engine
//!   triggers. The test uploads its plugin file
//!   (`dbine_test_jobs_plugin.py`), which stays in the plugin directory:
//!   InfluxDB has no API to delete it.
//!
//! Run with `cargo test -p dbine-driver-influxdb --test jobs -- --ignored`.
//! Each test makes its own `dbine_test_jobs_*` objects and removes them at
//! the end, also when an assertion fails.

use dbine_driver::{ConnectionConfig, JobOutcome, QueryOutcome, ServerJob, Session};
use serde_json::json;
use std::time::Duration;

async fn open(driver: &str, cfg: &ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_influxdb::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    assert!(d.capabilities().jobs);
    d.connect(cfg, db).await.unwrap()
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
    for _ in 0..80 {
        if let Some(j) = find(s, name).await.filter(|j| f(j)) {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{name}: {:?}", find(s, name).await);
}

async fn refuses(s: &mut Box<dyn Session>, bad: &[&str]) {
    for id in bad {
        assert!(s.job_detail(id).await.is_err(), "{id}");
        assert!(s.job_runs(id, 5).await.is_err(), "{id}");
        assert!(s.set_job_enabled(id, false).await.is_err(), "{id}");
        assert!(s.job_script(id).await.is_err(), "{id}");
    }
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

    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(m, format!("{}{path}", self.url))
            .header("Authorization", format!("Token {}", self.token))
    }

    /// Deletes every `dbine_test_jobs_*` task.
    async fn clean(&self) {
        let v: serde_json::Value = self
            .req(reqwest::Method::GET, "/api/v2/tasks")
            .query(&[("org", self.org.as_str()), ("limit", "500")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        for t in v["tasks"].as_array().into_iter().flatten() {
            if t["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("dbine_test_jobs_"))
            {
                let id = t["id"].as_str().unwrap();
                self.req(reqwest::Method::DELETE, &format!("/api/v2/tasks/{id}"))
                    .send()
                    .await
                    .unwrap();
            }
        }
    }

    async fn create(&self, flux: &str) {
        let orgs: serde_json::Value = self
            .req(reqwest::Method::GET, "/api/v2/orgs")
            .query(&[("org", self.org.as_str())])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let org_id = orgs["orgs"][0]["id"].as_str().unwrap().to_string();
        let r = self
            .req(reqwest::Method::POST, "/api/v2/tasks")
            .json(&json!({ "orgID": org_id, "flux": flux, "description": "dbine test" }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb2_tasks() {
    let Some(v2) = V2::env() else {
        eprintln!("DBINE_TEST_INFLUXDB_URL not set; skipping");
        return;
    };
    v2.clean().await;
    let body = {
        let cfg = v2.cfg();
        let v2 = V2::env().unwrap();
        tokio::spawn(async move { influxdb2_body(&v2, &cfg).await }).await
    };
    v2.clean().await;
    body.unwrap();
}

async fn influxdb2_body(v2: &V2, cfg: &ConnectionConfig) {
    v2.create(
        "option task = {name: \"dbine_test_jobs_hourly\", every: 1h, offset: 5m}\n\nfrom(bucket: \"test\") |> range(start: -1h) |> limit(n: 1)",
    )
    .await;
    // Slow enough to stop while it runs, without using memory: it waits on
    // an address that never answers.
    v2.create(
        "import \"http/requests\"\nimport \"array\"\noption task = {name: \"dbine_test_jobs_slow\", cron: \"0 3 * * *\"}\n\nd = requests.defaultConfig\nr = requests.get(url: \"http://10.255.255.1:81/\", config: {d with timeout: 60s})\narray.from(rows: [{_value: r.statusCode}]) |> yield()",
    )
    .await;
    let mut s = open("influxdb", cfg, None).await;
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    let t = find(&mut s, "dbine_test_jobs_hourly").await.unwrap();
    assert_eq!(t.kind, "Task");
    assert_eq!(t.group.as_deref(), Some("dbine"));
    assert_eq!(t.owner.as_deref(), Some("admin"));
    assert_eq!(t.description.as_deref(), Some("dbine test"));
    assert_eq!(t.schedule.as_deref(), Some("every 1h offset 5m"));
    assert!(t.enabled && t.can_run && t.can_stop && t.can_enable);
    assert!(t.has_history && t.has_script);
    let slow = find(&mut s, "dbine_test_jobs_slow").await.unwrap();
    assert_eq!(slow.schedule.as_deref(), Some("0 3 * * *"));

    let d = s.job_detail(&t.id).await.unwrap();
    assert_eq!(d.steps[0].kind.as_deref(), Some("Flux"));
    assert!(d
        .properties
        .iter()
        .any(|p| p.label == "Cada (every)" && p.value == "1h"));

    // Run now, then the run in the history. The task's last status changes
    // as the run ends; the run itself is listed a moment later, when the
    // server has written it to `_tasks`.
    s.run_job(&t.id).await.unwrap();
    let ran = wait_for(&mut s, "dbine_test_jobs_hourly", |j| {
        j.last_outcome == Some(JobOutcome::Succeeded) && !j.running && j.last_started.is_some()
    })
    .await;
    assert!(ran
        .last_started
        .as_deref()
        .is_some_and(|t| t.ends_with('Z')));
    let runs = s.job_runs(&t.id, 10).await.unwrap();
    eprintln!("{runs:#?}");
    assert_eq!(runs[0].outcome, JobOutcome::Succeeded);
    assert_eq!(runs[0].trigger.as_deref(), Some("manual"));
    assert!(runs[0].duration_ms.is_some());

    // Disable and enable; a disabled task isn't run.
    s.set_job_enabled(&t.id, false).await.unwrap();
    let off = find(&mut s, "dbine_test_jobs_hourly").await.unwrap();
    assert!(!off.enabled && !off.can_run);
    assert!(s.run_job(&t.id).await.is_err());
    s.set_job_enabled(&t.id, true).await.unwrap();
    assert!(
        find(&mut s, "dbine_test_jobs_hourly")
            .await
            .unwrap()
            .enabled
    );

    // Stop a run that's going.
    s.run_job(&slow.id).await.unwrap();
    wait_for(&mut s, "dbine_test_jobs_slow", |j| j.running).await;
    s.stop_job(&slow.id).await.unwrap();
    let stopped = wait_for(&mut s, "dbine_test_jobs_slow", |j| !j.running).await;
    assert_eq!(stopped.last_outcome, Some(JobOutcome::Canceled));
    assert!(s.stop_job(&slow.id).await.is_err(), "nothing left to stop");

    // The script is the task's Flux.
    let script = s.job_script(&t.id).await.unwrap();
    assert!(script.contains("dbine_test_jobs_hourly") && script.contains("every: 1h"));

    refuses(
        &mut s,
        &[
            "x' OR 1=1 --",
            "../../orgs",
            "119ff5d05e465000/runs",
            "0000000000000000",
        ],
    )
    .await;
    assert!(s.run_job("../../api/v2/buckets").await.is_err());
}

// ---------------------------------------------------------------- 1.x

async fn v1_body(url: &str, stats: bool) {
    let cfg = ConnectionConfig {
        driver: "influxdb1".into(),
        host: url.into(),
        ..Default::default()
    };
    let mut s = open("influxdb1", &cfg, None).await;
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE DATABASE dbine_test_jobs_db;
         CREATE CONTINUOUS QUERY dbine_test_jobs_cq ON dbine_test_jobs_db RESAMPLE EVERY 1s FOR 2s BEGIN SELECT count(v) INTO dbine_test_jobs_db.autogen.cpu_1s FROM dbine_test_jobs_db.autogen.cpu GROUP BY time(1s), * END",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    let cq = find(&mut s, "dbine_test_jobs_cq").await.unwrap();
    assert_eq!(cq.id, "\"dbine_test_jobs_db\".\"dbine_test_jobs_cq\"");
    assert_eq!(cq.kind, "Continuous query");
    assert_eq!(cq.schedule.as_deref(), Some("EVERY 1s FOR 2s"));
    assert!(cq.enabled && cq.has_script && !cq.can_run && !cq.can_stop && !cq.can_enable);
    assert!(list
        .notes
        .iter()
        .any(|n| n.contains("no se pueden ejecutar")));

    let d = s.job_detail(&cq.id).await.unwrap();
    assert_eq!(
        d.steps[0].target.as_deref(),
        Some("dbine_test_jobs_db.autogen.cpu_1s")
    );
    assert!(s.run_job(&cq.id).await.is_err());
    assert!(s.set_job_enabled(&cq.id, false).await.is_err());

    if stats {
        // Points for the CQ, then its runs.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let lines: String = (0..5)
            .map(|i| format!("cpu,host=a v={i} {}\n", now - i))
            .collect();
        let r = reqwest::Client::new()
            .post(format!("{url}/write?db=dbine_test_jobs_db&precision=s"))
            .body(lines)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        // The server records the first run after a second or two.
        wait_for(&mut s, "dbine_test_jobs_cq", |j| j.has_history).await;
        let mut runs = Vec::new();
        for _ in 0..40 {
            runs = s.job_runs(&cq.id, 5).await.unwrap();
            if !runs.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        eprintln!("{runs:#?}");
        assert!(!runs.is_empty() && runs.len() <= 5);
        assert!(runs[0].started.as_deref().is_some_and(|t| t.ends_with('Z')));
        assert!(runs.windows(2).all(|w| w[0].started >= w[1].started));
        // After `USE db.rp` the session sends that retention policy with
        // every query; the history still reads `_internal`'s own.
        for stmt in [
            "CREATE RETENTION POLICY dbine_test_jobs_rp ON dbine_test_jobs_db DURATION 1d REPLICATION 1",
            "USE dbine_test_jobs_db.dbine_test_jobs_rp",
        ] {
            let mut out = QueryOutcome::default();
            s.execute(stmt, 10, &mut out).await.unwrap();
        }
        let after_use = s.job_runs(&cq.id, 5).await.unwrap();
        assert!(!after_use.is_empty());
    } else {
        assert!(!cq.has_history);
        assert!(s.job_runs(&cq.id, 5).await.is_err());
    }

    // The script recreates it.
    let script = s.job_script(&cq.id).await.unwrap();
    assert!(script.starts_with("CREATE CONTINUOUS QUERY dbine_test_jobs_cq"));
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP CONTINUOUS QUERY dbine_test_jobs_cq ON dbine_test_jobs_db",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(find(&mut s, "dbine_test_jobs_cq").await.is_none());
    let mut out = QueryOutcome::default();
    s.execute(&script, 10, &mut out).await.unwrap();
    assert!(find(&mut s, "dbine_test_jobs_cq").await.is_some());

    refuses(
        &mut s,
        &[
            "x' OR 1=1 --",
            "\"dbine_test_jobs_db\".\"dbine_test_jobs_cq\"; DROP DATABASE dbine_test_jobs_db",
            "dbine_test_jobs_db.dbine_test_jobs_cq",
        ],
    )
    .await;
    let e = s
        .job_script("\"dbine_test_jobs_db\".\"x'; DROP DATABASE dbine_test_jobs_db\"")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no existe"), "{e}");
}

async fn v1_clean(url: &str) {
    reqwest::Client::new()
        .post(format!("{url}/query"))
        .form(&[("q", "DROP DATABASE dbine_test_jobs_db")])
        .send()
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb1_continuous_queries() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_URL not set; skipping");
        return;
    };
    v1_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move { v1_body(&u, false).await }).await;
    v1_clean(&url).await;
    body.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb1_continuous_query_runs() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_STATS_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_STATS_URL not set; skipping");
        return;
    };
    v1_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move { v1_body(&u, true).await }).await;
    v1_clean(&url).await;
    body.unwrap();
}

// ---------------------------------------------------------------- 3.x

const PLUGIN: &str = "dbine_test_jobs_plugin.py";
const PLUGIN_CODE: &str = "def process_scheduled_call(influxdb3_local, call_time, args=None):\n    if args and args.get(\"fail\") == \"yes\":\n        influxdb3_local.error(\"dbine falla a propósito\")\n        return\n    influxdb3_local.info(\"dbine tick\")\n";

async fn v3_clean(url: &str) {
    reqwest::Client::new()
        .delete(format!("{url}/api/v3/configure/database"))
        .query(&[("db", "dbine_test_jobs_db")])
        .send()
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb3_triggers() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB3_URL not set; skipping");
        return;
    };
    v3_clean(&url).await;
    let u = url.clone();
    let body = tokio::spawn(async move { v3_body(&u).await }).await;
    v3_clean(&url).await;
    body.unwrap();
}

async fn v3_body(url: &str) {
    let http = reqwest::Client::new();
    // The plugin: created, or replaced when an earlier run left it.
    let file = json!({ "plugin_name": PLUGIN, "content": PLUGIN_CODE });
    let r = http
        .post(format!("{url}/api/v3/plugins/files"))
        .json(&file)
        .send()
        .await
        .unwrap();
    if !r.status().is_success() {
        let r = http
            .put(format!("{url}/api/v3/plugins/files"))
            .json(&file)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }
    let r = http
        .post(format!("{url}/api/v3/configure/database"))
        .json(&json!({ "db": "dbine_test_jobs_db" }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    for (name, spec, args) in [
        ("dbine_test_jobs_tick", "every:1s", json!({ "k": "v" })),
        ("dbine_test_jobs_fail", "every:1s", json!({ "fail": "yes" })),
        ("dbine_test_jobs_night", "cron:0 0 3 * * *", json!({})),
    ] {
        let r = http
            .post(format!("{url}/api/v3/configure/processing_engine_trigger"))
            .json(&json!({
                "db": "dbine_test_jobs_db", "plugin_filename": PLUGIN, "trigger_name": name,
                "trigger_specification": spec, "trigger_arguments": args,
                "trigger_settings": { "run_async": false, "error_behavior": "log" },
                "disabled": name == "dbine_test_jobs_night",
            }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }

    let cfg = ConnectionConfig {
        driver: "influxdb3".into(),
        host: url.into(),
        ..Default::default()
    };
    let mut s = open("influxdb3", &cfg, None).await;
    let tick = wait_for(&mut s, "dbine_test_jobs_tick", |j| {
        j.last_outcome == Some(JobOutcome::Succeeded)
    })
    .await;
    eprintln!("{:#?}", s.jobs().await.unwrap());
    assert_eq!(tick.id, "\"dbine_test_jobs_db\".\"dbine_test_jobs_tick\"");
    assert_eq!(tick.kind, "Trigger");
    assert_eq!(tick.schedule.as_deref(), Some("every:1s"));
    assert!(tick.enabled && tick.can_enable && tick.has_history && tick.has_script);
    assert!(!tick.can_run && !tick.can_stop);
    let night = find(&mut s, "dbine_test_jobs_night").await.unwrap();
    assert!(!night.enabled);
    assert_eq!(night.schedule.as_deref(), Some("cron:0 0 3 * * *"));
    let fail = wait_for(&mut s, "dbine_test_jobs_fail", |j| {
        j.last_outcome == Some(JobOutcome::Failed)
    })
    .await;

    let d = s.job_detail(&tick.id).await.unwrap();
    assert!(d
        .properties
        .iter()
        .any(|p| p.label == "k" && p.value == "v"));
    assert!(d
        .properties
        .iter()
        .any(|p| p.label == "Se dispara" && p.value == "Cada 1s"));

    let runs = s.job_runs(&tick.id, 3).await.unwrap();
    eprintln!("{runs:#?}");
    assert!(!runs.is_empty() && runs.len() <= 3);
    assert!(runs.windows(2).all(|w| w[0].started >= w[1].started));
    assert!(runs
        .iter()
        .any(|r| r.outcome == JobOutcome::Succeeded && r.duration_ms.is_some()));
    let failed = s.job_runs(&fail.id, 3).await.unwrap();
    let f = failed
        .iter()
        .find(|r| r.outcome == JobOutcome::Failed)
        .expect("a failed run");
    assert!(
        f.message.as_deref().unwrap_or("").contains("a propósito"),
        "{f:?}"
    );

    // Disable and enable.
    s.set_job_enabled(&tick.id, false).await.unwrap();
    assert!(!find(&mut s, "dbine_test_jobs_tick").await.unwrap().enabled);
    s.set_job_enabled(&tick.id, true).await.unwrap();
    assert!(find(&mut s, "dbine_test_jobs_tick").await.unwrap().enabled);
    assert!(s.run_job(&tick.id).await.is_err());
    assert!(s.stop_job(&tick.id).await.is_err());

    // The script.
    let script = s.job_script(&tick.id).await.unwrap();
    eprintln!("{script}");
    assert!(script.contains("--trigger-spec 'every:1s'"), "{script}");
    assert!(script.contains("--trigger-arguments 'k=v'"), "{script}");
    let night = s.job_script(&night.id).await.unwrap();
    assert!(night.contains("--disabled"), "{night}");

    refuses(
        &mut s,
        &[
            "x' OR 1=1 --",
            "\"dbine_test_jobs_db\".\"dbine_test_jobs_tick\"&trigger_name=x",
            "dbine_test_jobs_db.dbine_test_jobs_tick",
        ],
    )
    .await;
    let e = s
        .set_job_enabled("\"dbine_test_jobs_db\".\"x' OR '1'='1\"", false)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no existe"), "{e}");
}

//! The Backup Service managed through the scheduler contract, against an
//! Enterprise cluster with the backup service (`DBINE_TEST_COUCHBASE_EE_URL`
//! with `DBINE_TEST_COUCHBASE_EE_MGMT_PORT`; see tests/jobs.rs for the
//! container). Skipped without it:
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_EE_URL=http://localhost:25993 DBINE_TEST_COUCHBASE_EE_MGMT_PORT=25991 \
//!   cargo test -p dbine-driver-couchbase --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! Every plan, repository and user the tests make is named
//! `dbine_test_sched_*` and is deleted at the end, whether the test passed
//! or not; the service's settings are put back as they were. DBine deletes
//! repositories without their backups (by design), and the service deletes
//! backups only with an archived repository, so the backups the test made
//! stay in the server's `/tmp/dbine_test_sched_archive` unless
//! `DBINE_TEST_COUCHBASE_EE_CONTAINER` names the container: then the
//! cleanup removes that directory (`docker exec … rm -rf`). Set it.

use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, Session};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const PLAN: &str = "dbine_test_sched_plan";
const PLAN2: &str = "dbine_test_sched_plan2";
const REPO: &str = "dbine_test_sched_repo";
const REPO2: &str = "dbine_test_sched_repo2";
const ARCHIVED: &str = "dbine_test_sched_arch";
const IMPORTED: &str = "dbine_test_sched_imp";
const RO_USER: &str = "dbine_test_sched_ro";
const RO_PASSWORD: &str = "Secreto_123";
const ARCHIVE_DIR: &str = "/tmp/dbine_test_sched_archive";

fn cfg() -> Option<(ConnectionConfig, String)> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_EE_URL").ok()?).ok()?;
    let mgmt = std::env::var("DBINE_TEST_COUCHBASE_EE_MGMT_PORT").unwrap_or_else(|_| "8091".into());
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), mgmt.clone());
    Some((c, format!("http://{}:{mgmt}", url.host_str()?)))
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_couchbase::drivers()[0]
        .connect(c, None)
        .await
        .expect("connect")
}

/// A request to the cluster manager as Administrator: status and body.
async fn api(mgmt: &str, m: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut rq = reqwest::Client::new()
        .request(m, format!("{mgmt}{path}"))
        .basic_auth("Administrator", Some("secreto1"));
    if let Some(b) = body {
        rq = rq.json(&b);
    }
    let r = rq.send().await.expect("http");
    let st = r.status().as_u16();
    let t = r.text().await.unwrap_or_default();
    (st, serde_json::from_str(&t).unwrap_or(Value::String(t)))
}

async fn backup(mgmt: &str, m: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
    api(mgmt, m, &format!("/_p/backup/api/v1{path}"), body).await
}

fn ours(v: &Value, key: &str) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.get(key).and_then(Value::as_str))
        .filter(|n| n.starts_with("dbine_test_sched_"))
        .map(str::to_string)
        .collect()
}

/// Deletes every `dbine_test_sched_*` plan, repository (with its backups)
/// and user; puts the settings back. Returns what is still there.
async fn cleanup(mgmt: &str, rotation: Option<Value>) -> Vec<String> {
    use reqwest::Method;
    let (_, active) = backup(mgmt, Method::GET, "/cluster/self/repository/active", None).await;
    for r in ours(&active, "id") {
        let (_, archived) =
            backup(mgmt, Method::GET, "/cluster/self/repository/archived", None).await;
        let mut to = format!("{r}_x");
        while ours(&archived, "id").contains(&to) {
            to.push('x');
        }
        backup(
            mgmt,
            Method::POST,
            &format!("/cluster/self/repository/active/{r}/archive"),
            Some(json!({"id": to})),
        )
        .await;
    }
    for state in ["archived", "imported"] {
        let (_, list) = backup(
            mgmt,
            Method::GET,
            &format!("/cluster/self/repository/{state}"),
            None,
        )
        .await;
        // An imported repository shares its data with an archived one:
        // only the archived one takes it along.
        let remove = state == "archived";
        for r in ours(&list, "id") {
            backup(
                mgmt,
                Method::DELETE,
                &format!("/cluster/self/repository/{state}/{r}?remove_repository={remove}"),
                None,
            )
            .await;
        }
    }
    let (_, plans) = backup(mgmt, Method::GET, "/plan", None).await;
    for p in ours(&plans, "name") {
        backup(mgmt, Method::DELETE, &format!("/plan/{p}"), None).await;
    }
    api(
        mgmt,
        Method::DELETE,
        &format!("/settings/rbac/users/local/{RO_USER}"),
        None,
    )
    .await;
    if let Some(size) = rotation {
        backup(
            mgmt,
            Method::POST,
            "/config",
            Some(json!({"history_rotation_size": size})),
        )
        .await;
    }
    if let Ok(container) = std::env::var("DBINE_TEST_COUCHBASE_EE_CONTAINER") {
        let _ = std::process::Command::new("docker")
            .args(["exec", &container, "rm", "-rf", ARCHIVE_DIR])
            .status();
    }
    let mut left = Vec::new();
    for state in ["active", "archived", "imported"] {
        let (_, list) = backup(
            mgmt,
            Method::GET,
            &format!("/cluster/self/repository/{state}"),
            None,
        )
        .await;
        left.extend(ours(&list, "id"));
    }
    let (_, plans) = backup(mgmt, Method::GET, "/plan", None).await;
    left.extend(ours(&plans, "name"));
    let (st, _) = api(
        mgmt,
        Method::GET,
        &format!("/settings/rbac/users/local/{RO_USER}"),
        None,
    )
    .await;
    if st == 200 {
        left.push(RO_USER.into());
    }
    left
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

/// A new list row: the list's defaults, then `fields`.
fn new_row(form: &ObjectForm, list: &str, fields: Value) -> BTreeMap<String, Value> {
    let l = form
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .find(|l| l.key == list)
        .unwrap_or_else(|| panic!("list {list}"));
    let mut row: BTreeMap<String, Value> = l
        .fields
        .iter()
        .filter_map(|f| Some((f.key.clone(), f.default.clone()?)))
        .collect();
    for (k, v) in fields.as_object().unwrap() {
        row.insert(k.clone(), v.clone());
    }
    row
}

fn set(v: &mut FormValues, k: &str, x: Value) {
    v.values.insert(k.into(), x);
}

async fn save(s: &mut Box<dyn Session>, kind: &str, id: Option<&str>, v: &FormValues) -> String {
    let script = s.scheduler_script(kind, id, v).await.expect("script");
    eprintln!("--- {kind} {id:?}\n{script}");
    let r = s.scheduler_save(kind, id, v).await;
    r.unwrap_or_else(|e| panic!("save {kind}: {e:?}\n{script}"))
        .expect("an id")
}

async fn listed(s: &mut Box<dyn Session>, kind: &str) -> Vec<String> {
    s.scheduler_items(kind)
        .await
        .expect("items")
        .items
        .into_iter()
        .map(|i| i.id)
        .collect()
}

/// Waits for the newest task of the repository named like `prefix` to end.
async fn task_done(mgmt: &str, repo: &str, prefix: &str, before: &[String]) -> String {
    let start = Instant::now();
    loop {
        let (_, h) = backup(
            mgmt,
            reqwest::Method::GET,
            &format!("/cluster/self/repository/active/{repo}/taskHistory?limit=20"),
            None,
        )
        .await;
        if let Some(t) = h.as_array().into_iter().flatten().find(|t| {
            let n = t["task_name"].as_str().unwrap_or_default();
            n.starts_with(prefix) && !before.iter().any(|b| b == n)
        }) {
            let st = t["status"].as_str().unwrap_or_default().to_string();
            if st != "running" && st != "waiting" {
                return st;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "the {prefix} task never ended"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn task_names(mgmt: &str, repo: &str) -> Vec<String> {
    let (_, h) = backup(
        mgmt,
        reqwest::Method::GET,
        &format!("/cluster/self/repository/active/{repo}/taskHistory?limit=50"),
        None,
    )
    .await;
    h.as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["task_name"].as_str().map(str::to_string))
        .collect()
}

async fn body(c: ConnectionConfig, mgmt: String) {
    use reqwest::Method;
    let mut s = open(&c).await;

    // What the service has.
    let info = s.scheduler_info().await.expect("info");
    let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "plan", "archived", "imported", "settings"]);
    assert!(info.kinds.iter().all(|k| k.can_edit || k.key != "plan"));
    assert!(info.logs, "8.0 has the event log");
    // A repository starts at a task of its plan; nothing to clear; a full
    // backup and a merge of every backup now.
    let job = &info.kinds[0];
    assert!(job.run_from_step && !job.clear_history, "{job:?}");
    let acts: Vec<&str> = job.actions.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(acts, ["backup_now", "merge_now"]);

    // A built-in plan: read-only, and saving or deleting it is refused.
    assert!(listed(&mut s, "plan")
        .await
        .contains(&"_hourly_backups".to_string()));
    let f = s
        .scheduler_form("plan", Some("_hourly_backups"))
        .await
        .expect("form");
    assert!(f.read_only && f.notes.iter().any(|n| n.contains("viene con Couchbase")));
    let v = values_of(&f);
    assert!(s
        .scheduler_save("plan", Some("_hourly_backups"), &v)
        .await
        .is_err());
    assert!(s.scheduler_delete("plan", "_hourly_backups").await.is_err());
    let (_, p) = backup(&mgmt, Method::GET, "/plan/_hourly_backups", None).await;
    assert_eq!(p["default"], true, "the built-in plan is untouched");

    // A plan: an hourly full backup, a weekly merge and a daily prune.
    let f = s.scheduler_form("plan", None).await.expect("new plan");
    let mut v = values_of(&f);
    set(&mut v, "name", json!(PLAN));
    set(&mut v, "description", json!("dbine test"));
    set(&mut v, "services", json!(["data", "gsi"]));
    v.lists.insert(
        "tasks".into(),
        vec![
            new_row(&f, "tasks", json!({"name": "hourly", "task_type": "BACKUP", "full_backup": true, "frequency": 1, "period": "HOURS", "time": "00:15:00"})),
            new_row(&f, "tasks", json!({"name": "weekly_merge", "task_type": "MERGE", "frequency": 1, "period": "SUNDAY", "time": "23:30:00", "offset_start": 0, "offset_end": 6})),
            new_row(&f, "tasks", json!({"name": "prune", "task_type": "PRUNE", "frequency": 1, "period": "DAYS", "time": "02:00:00"})),
        ],
    );
    assert_eq!(save(&mut s, "plan", None, &v).await, PLAN);
    let (st, p) = backup(&mgmt, Method::GET, &format!("/plan/{PLAN}"), None).await;
    assert_eq!(st, 200, "{p}");
    assert_eq!(p["description"], "dbine test");
    assert_eq!(p["services"], json!(["data", "gsi"]));
    assert_eq!(p["tasks"][0]["full_backup"], true);
    assert_eq!(p["tasks"][0]["schedule"]["time"], "00:15");
    assert_eq!(p["tasks"][1]["merge_options"]["offset_end"], 6);
    assert_eq!(p["tasks"][2]["task_type"], "PRUNE");
    // A second one with the same name is refused before it's sent.
    assert!(s.scheduler_script("plan", None, &v).await.is_err());

    // Edit it: description, every service, a task changed, one removed.
    let f = s
        .scheduler_form("plan", Some(PLAN))
        .await
        .expect("plan form");
    assert!(!f.read_only, "{:?}", f.notes);
    let mut v = values_of(&f);
    let unchanged = s.scheduler_script("plan", Some(PLAN), &v).await.unwrap();
    assert!(unchanged.contains("no hay cambios"), "{unchanged}");
    set(&mut v, "description", json!("dbine test 2"));
    set(&mut v, "services", Value::Null);
    let tasks = v.lists.get_mut("tasks").unwrap();
    tasks[0].insert("frequency".into(), json!(2));
    tasks.pop();
    assert_eq!(save(&mut s, "plan", Some(PLAN), &v).await, PLAN);
    let (_, p) = backup(&mgmt, Method::GET, &format!("/plan/{PLAN}"), None).await;
    assert_eq!(p["description"], "dbine test 2");
    assert_eq!(p["services"], Value::Null);
    assert_eq!(p["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(p["tasks"][0]["schedule"]["frequency"], 2);

    // A second plan to move the repository to.
    let f = s.scheduler_form("plan", None).await.unwrap();
    let mut v = values_of(&f);
    set(&mut v, "name", json!(PLAN2));
    v.lists.insert(
        "tasks".into(),
        vec![new_row(&f, "tasks", json!({"name": "daily", "task_type": "BACKUP", "frequency": 1, "period": "DAYS", "time": "03:00:00"}))],
    );
    save(&mut s, "plan", None, &v).await;

    // An active repository, paused, with a retention.
    let f = s.scheduler_form("job", None).await.expect("new repo");
    assert!(f.choices["plan"].iter().any(|(p, _)| p == PLAN));
    let mut v = values_of(&f);
    set(&mut v, "name", json!(REPO));
    set(&mut v, "plan", json!(PLAN));
    set(&mut v, "state", json!("paused"));
    set(&mut v, "archive", json!(ARCHIVE_DIR));
    set(&mut v, "retention", json!(30));
    let id = save(&mut s, "job", None, &v).await;
    assert_eq!(id, format!("repo:{REPO}"));
    let (_, r) = backup(
        &mgmt,
        Method::GET,
        &format!("/cluster/self/repository/active/{REPO}"),
        None,
    )
    .await;
    assert_eq!(r["state"], "paused");
    assert_eq!(r["plan_name"], PLAN);
    let (_, i) = backup(
        &mgmt,
        Method::GET,
        &format!("/cluster/self/repository/active/{REPO}/info?limit=1"),
        None,
    )
    .await;
    assert_eq!(i["default_retention"], 30);
    // The tree's job rows are the jobs' ids.
    assert!(listed(&mut s, "job").await.contains(&id));
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));

    // The plan it uses can't change or go.
    let f = s.scheduler_form("plan", Some(PLAN)).await.unwrap();
    assert!(
        f.read_only && f.notes.iter().any(|n| n.contains(REPO)),
        "{:?}",
        f.notes
    );
    assert!(s.scheduler_delete_script("plan", PLAN).await.is_err());
    // The tree offers neither an edit nor a delete on it, nor on a
    // built-in plan; the plan nobody uses, both.
    let plans = s.scheduler_items("plan").await.unwrap().items;
    for (p, fixed) in [(PLAN, true), ("_hourly_backups", true), (PLAN2, false)] {
        let it = plans.iter().find(|i| i.id == p).unwrap();
        let want = if fixed { Some(false) } else { None };
        assert_eq!((it.can_edit, it.can_delete), (want, want), "{p}");
    }

    // Its form: what the service says of it.
    let f = s.scheduler_form("job", Some(&id)).await.expect("repo form");
    assert!(!f.read_only);
    assert_eq!(f.values["state"], "paused");
    assert_eq!(f.values["archive"], ARCHIVE_DIR);
    assert_eq!(f.values["retention"], 30);
    let tasks = &f.sections.iter().find(|s| s.key == "tasks").unwrap().lists[0].rows;
    assert_eq!(tasks.len(), 2);

    // Edit: another plan, active, a longer retention.
    let mut v = values_of(&f);
    set(&mut v, "plan", json!(PLAN2));
    set(&mut v, "state", json!("active"));
    set(&mut v, "retention", json!(45));
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(
        script.contains(&format!("/plan/{PLAN2}")) && script.contains("retentionSettings"),
        "{script}"
    );
    assert!(
        !script.contains("/pause"),
        "it was paused already: {script}"
    );
    save(&mut s, "job", Some(&id), &v).await;
    let (_, r) = backup(
        &mgmt,
        Method::GET,
        &format!("/cluster/self/repository/active/{REPO}"),
        None,
    )
    .await;
    assert_eq!(r["state"], "active");
    assert_eq!(r["plan_name"], PLAN2);
    let (_, i) = backup(
        &mgmt,
        Method::GET,
        &format!("/cluster/self/repository/active/{REPO}/info?limit=1"),
        None,
    )
    .await;
    assert_eq!(i["default_retention"], 45);
    // And back to the first plan while active: paused around the change.
    let f = s.scheduler_form("job", Some(&id)).await.unwrap();
    let mut v = values_of(&f);
    set(&mut v, "plan", json!(PLAN));
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(
        script.contains("/pause") && script.contains("/resume"),
        "{script}"
    );
    save(&mut s, "job", Some(&id), &v).await;
    let (_, r) = backup(
        &mgmt,
        Method::GET,
        &format!("/cluster/self/repository/active/{REPO}"),
        None,
    )
    .await;
    assert_eq!(
        (r["state"].as_str(), r["plan_name"].as_str()),
        (Some("active"), Some(PLAN))
    );

    // Run the plan's tasks now: a full backup, another, a merge of both.
    let before = task_names(&mgmt, REPO).await;
    s.run_job_from(&id, "hourly").await.expect("backup now");
    assert_eq!(task_done(&mgmt, REPO, "BACKUP", &before).await, "done");
    let before = task_names(&mgmt, REPO).await;
    s.run_job_from(&id, "hourly").await.expect("backup again");
    assert_eq!(task_done(&mgmt, REPO, "BACKUP", &before).await, "done");
    let before = task_names(&mgmt, REPO).await;
    s.run_job_from(&id, "weekly_merge")
        .await
        .expect("merge now");
    let st = task_done(&mgmt, REPO, "MERGE", &before).await;
    assert!(st == "done" || st == "skipped", "{st}");
    assert!(s.run_job_from(&id, "nope").await.is_err());
    // No history to clear.
    assert!(matches!(
        s.clear_job_history(&id).await,
        Err(Error::Unsupported(_))
    ));

    // The actions: a full backup now, then every backup merged into one.
    let it = s
        .scheduler_items("job")
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id == id)
        .unwrap();
    assert_eq!(
        it.actions,
        Some(vec!["backup_now".to_string(), "merge_now".to_string()])
    );
    let sc = s
        .scheduler_action_script("job", &id, "backup_now")
        .await
        .unwrap();
    assert!(
        sc.contains(&format!("/active/{REPO}/backup")) && sc.contains("\"full_backup\": true"),
        "{sc}"
    );
    let before = task_names(&mgmt, REPO).await;
    s.scheduler_action("job", &id, "backup_now")
        .await
        .expect("full backup now");
    assert_eq!(task_done(&mgmt, REPO, "BACKUP", &before).await, "done");
    let info_of = || async {
        backup(
            &mgmt,
            Method::GET,
            &format!("/cluster/self/repository/active/{REPO}/info"),
            None,
        )
        .await
        .1
    };
    let i = info_of().await;
    let n = i["backups"].as_array().map_or(0, Vec::len);
    assert!(n >= 2, "{i}");
    assert!(
        i["backups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "FULL"),
        "{i}"
    );
    let sc = s
        .scheduler_action_script("job", &id, "merge_now")
        .await
        .unwrap();
    assert!(
        sc.contains(&format!("/active/{REPO}/merge")) && sc.contains("\"offset_start\": 0"),
        "{sc}"
    );
    let before = task_names(&mgmt, REPO).await;
    s.scheduler_action("job", &id, "merge_now")
        .await
        .expect("merge now");
    assert_eq!(task_done(&mgmt, REPO, "MERGE", &before).await, "done");
    let i = info_of().await;
    assert_eq!(i["backups"].as_array().map_or(0, Vec::len), 1, "{i}");
    // One backup left: nothing to merge. No other action, nor on a plan.
    let e = s
        .scheduler_action_script("job", &id, "merge_now")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("un solo backup"), "{e}");
    assert!(s
        .scheduler_action_script("job", &id, "archive")
        .await
        .is_err());
    assert!(s
        .scheduler_action_script("plan", PLAN, "backup_now")
        .await
        .is_err());

    // The service's events.
    let logs = s.scheduler_logs().await.expect("logs");
    assert!(
        logs[0]
            .rows
            .iter()
            .any(|r| r[3].as_str().unwrap_or_default().contains(REPO)),
        "{:?}",
        logs[0].rows.first()
    );

    // Archive another repository through the archived kind.
    let f = s.scheduler_form("job", None).await.unwrap();
    let mut v = values_of(&f);
    set(&mut v, "name", json!(REPO2));
    set(&mut v, "plan", json!(PLAN2));
    set(&mut v, "archive", json!(ARCHIVE_DIR));
    save(&mut s, "job", None, &v).await;
    let (_, r2) = backup(
        &mgmt,
        Method::GET,
        &format!("/cluster/self/repository/active/{REPO2}"),
        None,
    )
    .await;
    let uuid = r2["repo"].as_str().unwrap().to_string();
    let f = s.scheduler_form("archived", None).await.unwrap();
    assert!(f.choices["active"].iter().any(|(n, _)| n == REPO2));
    let mut v = values_of(&f);
    set(&mut v, "source", json!(REPO2));
    set(&mut v, "name", json!(ARCHIVED));
    assert_eq!(save(&mut s, "archived", None, &v).await, ARCHIVED);
    assert!(listed(&mut s, "archived")
        .await
        .contains(&ARCHIVED.to_string()));
    assert!(!listed(&mut s, "job")
        .await
        .contains(&format!("repo:{REPO2}")));
    let f = s.scheduler_form("archived", Some(ARCHIVED)).await.unwrap();
    assert!(f.read_only && f.values["plan"] == PLAN2);

    // Import its data again.
    let f = s.scheduler_form("imported", None).await.unwrap();
    let mut v = values_of(&f);
    set(&mut v, "name", json!(IMPORTED));
    set(&mut v, "repo", json!(uuid));
    set(&mut v, "archive", json!(ARCHIVE_DIR));
    assert_eq!(save(&mut s, "imported", None, &v).await, IMPORTED);
    let f = s.scheduler_form("imported", Some(IMPORTED)).await.unwrap();
    assert!(f.read_only && f.values["uuid"] == uuid);
    let script = s
        .scheduler_delete_script("imported", IMPORTED)
        .await
        .unwrap();
    assert!(script.contains("remove_repository=false"), "{script}");
    s.scheduler_delete("imported", IMPORTED)
        .await
        .expect("delete imported");
    assert!(!listed(&mut s, "imported")
        .await
        .contains(&IMPORTED.to_string()));
    s.scheduler_delete("archived", ARCHIVED)
        .await
        .expect("delete archived");
    assert!(!listed(&mut s, "archived")
        .await
        .contains(&ARCHIVED.to_string()));

    // Delete the active one: archived, then deleted from the service.
    let script = s.scheduler_delete_script("job", &id).await.unwrap();
    assert!(
        script.contains("/archive") && script.contains("DELETE"),
        "{script}"
    );
    s.scheduler_delete("job", &id).await.expect("delete repo");
    assert!(!listed(&mut s, "job").await.contains(&id));
    assert!(!listed(&mut s, "archived")
        .await
        .iter()
        .any(|a| a.starts_with(REPO)));

    // Plans free again: deleted.
    for p in [PLAN, PLAN2] {
        s.scheduler_delete("plan", p).await.expect("delete plan");
    }
    assert!(!listed(&mut s, "plan")
        .await
        .iter()
        .any(|p| p.starts_with("dbine_test_sched_")));

    // The service's settings.
    let f = s.scheduler_form("settings", Some("service")).await.unwrap();
    let mut v = values_of(&f);
    let was = f.values["history_rotation_size"].as_i64().unwrap();
    let to = if was == 60 { 61 } else { 60 };
    set(&mut v, "history_rotation_size", json!(to));
    save(&mut s, "settings", Some("service"), &v).await;
    let (_, cfg) = backup(&mgmt, Method::GET, "/config", None).await;
    assert_eq!(cfg["history_rotation_size"], to);
    set(&mut v, "history_rotation_size", json!(3));
    assert!(s
        .scheduler_script("settings", Some("service"), &v)
        .await
        .is_err());
    set(&mut v, "history_rotation_size", json!(was));
    save(&mut s, "settings", Some("service"), &v).await;

    // Ids that aren't the service's are refused before anything is sent.
    for (kind, bad) in [
        ("job", "repo:../../plan/x"),
        ("job", "repo:a b"),
        ("job", "dbine_test_sched_repo"),
        ("plan", "../x"),
        ("plan", "a?b"),
        ("archived", "a/b"),
        ("imported", ""),
        ("settings", "other"),
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

async fn read_only_body(c: ConnectionConfig, mgmt: String) {
    // The user's password takes a moment to be accepted.
    let mut c = c;
    c.username = Some(RO_USER.into());
    c.password = Some(RO_PASSWORD.into());
    let start = Instant::now();
    let mut s = loop {
        match dbine_driver_couchbase::drivers()[0].connect(&c, None).await {
            Ok(s) => break s,
            Err(e) => {
                assert!(start.elapsed() < Duration::from_secs(20), "connect: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    let info = s.scheduler_info().await.expect("info");
    assert_eq!(info.kinds.len(), 5);
    assert!(info
        .kinds
        .iter()
        .all(|k| !k.can_create && !k.can_edit && !k.can_delete));
    assert!(
        info.notes.iter().any(|n| n.contains("no cambiarlo")),
        "{:?}",
        info.notes
    );
    // No actions, nor a task started now.
    assert!(info
        .kinds
        .iter()
        .all(|k| k.actions.is_empty() && !k.run_from_step));
    let f = s.scheduler_form("plan", None).await.expect("form");
    assert!(f.read_only);
    let mut v = values_of(&f);
    v.values
        .insert("name".into(), json!("dbine_test_sched_ro_plan"));
    assert!(s.scheduler_save("plan", None, &v).await.is_err());
    let (_, plans) = backup(&mgmt, reqwest::Method::GET, "/plan", None).await;
    assert!(ours(&plans, "name").is_empty());
}

async fn rotation(mgmt: &str) -> Option<Value> {
    let (_, cfg) = backup(mgmt, reqwest::Method::GET, "/config", None).await;
    cfg.get("history_rotation_size").cloned()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn the_backup_service_managed_whole() {
    let Some((c, mgmt)) = cfg() else {
        eprintln!("DBINE_TEST_COUCHBASE_EE_URL not set; skipping");
        return;
    };
    let size = rotation(&mgmt).await;
    let left = cleanup(&mgmt, None).await;
    assert!(left.is_empty(), "leftovers that can't be removed: {left:?}");
    let r = tokio::spawn(body(c, mgmt.clone())).await;
    let left = cleanup(&mgmt, size).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
    assert!(left.is_empty(), "left behind: {left:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_read_only_admin_sees_but_cannot_change() {
    let Some((c, mgmt)) = cfg() else {
        eprintln!("DBINE_TEST_COUCHBASE_EE_URL not set; skipping");
        return;
    };
    cleanup(&mgmt, None).await;
    let r = reqwest::Client::new()
        .put(format!("{mgmt}/settings/rbac/users/local/{RO_USER}"))
        .basic_auth("Administrator", Some("secreto1"))
        .form(&[("password", RO_PASSWORD), ("roles", "ro_admin")])
        .send()
        .await
        .expect("http");
    assert_eq!(r.status().as_u16(), 200);
    let r = tokio::spawn(read_only_body(c, mgmt.clone())).await;
    let left = cleanup(&mgmt, None).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
    assert!(left.is_empty(), "left behind: {left:?}");
}

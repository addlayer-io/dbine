//! The scheduler designer against a real Snowflake account (no emulator;
//! the same variables as tests/jobs.rs, a role that can create a database,
//! tasks and alerts, and a warehouse):
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=… DBINE_TEST_SNOWFLAKE_USER=… DBINE_TEST_SNOWFLAKE_TOKEN=… \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test scheduler -- --ignored --nocapture
//! ```
//!
//! It makes its own database (`DBINE_TEST_SCHED_DB`) and every object in it
//! is named `DBINE_TEST_SCHED_*`; the database is dropped at the end, also
//! when an assertion fails. Tasks are left suspended (or resumed only on a
//! schedule that doesn't come during the test), so nothing runs on the
//! warehouse.

use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, QueryOutcome, Session};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const DB: &str = "DBINE_TEST_SCHED_DB";
const ROOT: &str = "DBINE_TEST_SCHED_ROOT";
const CHILD: &str = "DBINE_TEST_SCHED_CHILD";
const CHILD2: &str = "DBINE_TEST_SCHED_CHILD2";
const FIN: &str = "DBINE_TEST_SCHED_FIN";
const ALERT: &str = "DBINE_TEST_SCHED_ALERT";
const GRAPH: &str = "DBINE_TEST_SCHED_GRAPH";
const G_ROOT: &str = "DBINE_TEST_SCHED_G_ROOT";

fn config() -> Option<ConnectionConfig> {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok();
    let mut options = std::collections::HashMap::from([
        ("account".to_string(), var("ACCOUNT")?),
        ("token".to_string(), var("TOKEN")?),
    ]);
    options.insert("warehouse".into(), var("WAREHOUSE")?);
    Some(ConnectionConfig {
        driver: "snowflake".into(),
        username: var("USER"),
        database: DB.into(),
        options: options.into_iter().collect(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 1000, &mut out).await.map(|_| out)
}

/// The rows of a statement as maps of lower-case column → text.
async fn rows(s: &mut Box<dyn Session>, q: &str) -> Vec<BTreeMap<String, String>> {
    let out = run(s, q).await.unwrap();
    let set = out.results.last().expect("a result");
    set.rows
        .iter()
        .map(|r| {
            set.columns
                .iter()
                .zip(r.iter())
                .map(|(c, v)| {
                    (
                        c.name.to_ascii_lowercase(),
                        match v {
                            Value::String(s) => s.clone(),
                            Value::Null => String::new(),
                            v => v.to_string(),
                        },
                    )
                })
                .collect()
        })
        .collect()
}

/// What the designer sends back for a form as it came.
fn values_of(f: &ObjectForm) -> FormValues {
    FormValues {
        values: f.values.clone(),
        lists: f
            .sections
            .iter()
            .flat_map(|s| s.lists.iter())
            .map(|l| (l.key.clone(), l.rows.clone()))
            .collect(),
    }
}

fn set(v: &mut FormValues, k: &str, value: Value) {
    v.values.insert(k.into(), value);
}

fn id(name: &str) -> String {
    format!("\"{DB}\".\"PUBLIC\".\"{name}\"")
}

fn schema() -> String {
    format!("\"{DB}\".\"PUBLIC\"")
}

async fn task(s: &mut Box<dyn Session>, name: &str) -> BTreeMap<String, String> {
    rows(
        s,
        &format!("SHOW TASKS LIKE '{name}' IN SCHEMA {DB}.PUBLIC"),
    )
    .await
    .into_iter()
    .find(|r| r["name"] == name)
    .unwrap_or_else(|| panic!("{name} is there"))
}

async fn task_names(s: &mut Box<dyn Session>) -> Vec<String> {
    let mut n: Vec<String> = rows(s, &format!("SHOW TASKS IN SCHEMA {DB}.PUBLIC"))
        .await
        .into_iter()
        .map(|r| r["name"].clone())
        .collect();
    n.sort();
    n
}

async fn tasks(s: &mut Box<dyn Session>, wh: &str) {
    let info = s.scheduler_info().await.unwrap();
    let kinds: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
    // Task graph objects are a preview: offered only where the account has
    // them.
    assert!(
        kinds == ["job", "alert", "task_graph"] || kinds == ["job", "alert"],
        "{kinds:?}"
    );

    // A new graph: root, child and finalizer, created suspended.
    let form = s.scheduler_form("job", None).await.unwrap();
    let mut v = values_of(&form);
    set(&mut v, "schema", json!(schema()));
    set(&mut v, "name", json!(ROOT));
    set(&mut v, "compute", json!("warehouse"));
    set(&mut v, "warehouse", json!(wh));
    set(&mut v, "schedule", json!("cron"));
    set(&mut v, "cron", json!("0 3 1 1 *"));
    set(&mut v, "timezone", json!("UTC"));
    set(&mut v, "definition", json!("SELECT 1"));
    set(&mut v, "comment", json!("dbine's test"));
    set(&mut v, "config", json!(r#"{"env": "test"}"#));
    set(&mut v, "enabled", json!(false));
    v.lists.insert(
        "session_params".into(),
        vec![serde_json::from_value(json!({"name": "TIMEZONE", "value": "UTC"})).unwrap()],
    );
    v.lists.insert(
        "tasks".into(),
        vec![
            serde_json::from_value(json!({
                "name": CHILD, "role": "child", "after_root": true, "after": [],
                "compute": "warehouse", "warehouse": wh, "definition": "SELECT 2",
                "enabled": true
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "name": FIN, "role": "finalizer", "compute": "warehouse", "warehouse": wh,
                "definition": "SELECT 3", "enabled": true
            }))
            .unwrap(),
        ],
    );
    let script = s.scheduler_script("job", None, &v).await.unwrap();
    eprintln!("{script}");
    assert!(
        script.contains(&format!("CREATE TASK {}", id(ROOT))),
        "{script}"
    );
    let saved = s.scheduler_save("job", None, &v).await.unwrap();
    assert_eq!(saved.as_deref(), Some(id(ROOT).as_str()));
    assert_eq!(task_names(s).await, [CHILD, FIN, ROOT]);
    let root = task(s, ROOT).await;
    assert_eq!(root["state"], "suspended");
    assert_eq!(root["comment"], "dbine's test");
    assert_eq!(root["schedule"], "USING CRON 0 3 1 1 * UTC");
    let child = task(s, CHILD).await;
    assert!(child["predecessors"].contains(ROOT), "{child:?}");
    assert_eq!(child["state"], "started");
    assert!(task(s, FIN).await["task_relations"].contains("FinalizedRootTask"));

    // The jobs list and the designer's items share the id.
    let jobs = s.jobs().await.unwrap();
    assert!(jobs.jobs.iter().any(|j| j.id == id(ROOT)));
    let items = s.scheduler_items("job").await.unwrap();
    assert!(items.items.iter().any(|i| i.id == id(ROOT)));

    // Read back: nothing to save.
    let form = s.scheduler_form("job", Some(&id(ROOT))).await.unwrap();
    assert!(!form.read_only, "{:?}", form.notes);
    let script = s
        .scheduler_script("job", Some(&id(ROOT)), &values_of(&form))
        .await
        .unwrap();
    assert!(script.contains("No hay cambios"), "{script}");

    // Edit: comment, the child's SQL, a new child after it, a parameter.
    let mut v = values_of(&form);
    set(&mut v, "comment", json!("changed"));
    v.lists.get_mut("session_params").unwrap()[0].insert("value".into(), json!("Europe/Madrid"));
    let rows_ = v.lists.get_mut("tasks").unwrap();
    rows_
        .iter_mut()
        .find(|r| r["name"] == json!(CHILD))
        .unwrap()
        .insert("definition".into(), json!("SELECT 22"));
    rows_.push(
        serde_json::from_value(json!({
            "name": CHILD2, "role": "child", "after_root": false, "after": [CHILD],
            "compute": "warehouse", "warehouse": wh, "definition": "SELECT 4", "enabled": true
        }))
        .unwrap(),
    );
    eprintln!(
        "{}",
        s.scheduler_script("job", Some(&id(ROOT)), &v)
            .await
            .unwrap()
    );
    s.scheduler_save("job", Some(&id(ROOT)), &v).await.unwrap();
    assert_eq!(task(s, ROOT).await["comment"], "changed");
    assert_eq!(task(s, CHILD).await["definition"], "SELECT 22");
    let c2 = task(s, CHILD2).await;
    assert!(c2["predecessors"].contains(CHILD), "{c2:?}");
    assert_eq!(c2["state"], "started");
    let tz = rows(
        s,
        &format!("SHOW PARAMETERS LIKE 'TIMEZONE' IN TASK {}", id(ROOT)),
    )
    .await;
    assert_eq!(tz[0]["value"], "Europe/Madrid");

    // Reshape: CHILD2 leaves, the finalizer becomes a child of CHILD.
    let form = s.scheduler_form("job", Some(&id(ROOT))).await.unwrap();
    let mut v = values_of(&form);
    let rows_ = v.lists.get_mut("tasks").unwrap();
    rows_.retain(|r| r["name"] != json!(CHILD2));
    let fin = rows_.iter_mut().find(|r| r["name"] == json!(FIN)).unwrap();
    fin.insert("role".into(), json!("child"));
    fin.insert("after_root".into(), json!(false));
    fin.insert("after".into(), json!([CHILD]));
    s.scheduler_save("job", Some(&id(ROOT)), &v).await.unwrap();
    assert_eq!(task_names(s).await, [CHILD, FIN, ROOT]);
    let fin = task(s, FIN).await;
    assert!(fin["predecessors"].contains(CHILD), "{fin:?}");
    assert!(!fin["task_relations"].contains("FinalizedRootTask"));

    // Started, then changed: suspended for the change and resumed.
    let form = s.scheduler_form("job", Some(&id(ROOT))).await.unwrap();
    let mut v = values_of(&form);
    set(&mut v, "enabled", json!(true));
    s.scheduler_save("job", Some(&id(ROOT)), &v).await.unwrap();
    assert_eq!(task(s, ROOT).await["state"], "started");
    set(&mut v, "comment", json!("while started"));
    let script = s
        .scheduler_script("job", Some(&id(ROOT)), &v)
        .await
        .unwrap();
    assert!(script.contains("SUSPEND;"), "{script}");
    s.scheduler_save("job", Some(&id(ROOT)), &v).await.unwrap();
    let root = task(s, ROOT).await;
    assert_eq!(root["comment"], "while started");
    assert_eq!(root["state"], "started");
    set(&mut v, "enabled", json!(false));
    s.scheduler_save("job", Some(&id(ROOT)), &v).await.unwrap();
    assert_eq!(task(s, ROOT).await["state"], "suspended");

    // Start at a child: only a retry of a failed run (none here).
    assert!(s.run_job_from(&id(ROOT), CHILD).await.is_err());
    assert!(matches!(
        s.clear_job_history(&id(ROOT)).await,
        Err(Error::Unsupported(_))
    ));
    match s.scheduler_logs().await {
        Ok(t) => assert_eq!(t.len(), 2),
        Err(e) => assert!(e.to_string().contains("warehouse"), "{e}"),
    }

    // Crafted ids are refused before anything runs.
    let evil = format!("x'); DROP DATABASE {DB}; --");
    assert!(s.scheduler_form("job", Some(&evil)).await.is_err());
    assert!(s.scheduler_delete("job", &evil).await.is_err());
    assert!(s.scheduler_delete("alert", &evil).await.is_err());

    // Delete: the whole graph.
    let script = s.scheduler_delete_script("job", &id(ROOT)).await.unwrap();
    assert_eq!(script.matches("DROP TASK").count(), 3, "{script}");
    s.scheduler_delete("job", &id(ROOT)).await.unwrap();
    assert!(task_names(s).await.is_empty());
}

async fn alerts(s: &mut Box<dyn Session>, wh: &str) {
    let form = s.scheduler_form("alert", None).await.unwrap();
    let mut v = values_of(&form);
    set(&mut v, "schema", json!(schema()));
    set(&mut v, "name", json!(ALERT));
    set(&mut v, "compute", json!("warehouse"));
    set(&mut v, "warehouse", json!(wh));
    set(&mut v, "schedule", json!("interval"));
    set(&mut v, "every", json!(60));
    set(&mut v, "condition", json!("SELECT 1 WHERE FALSE"));
    set(&mut v, "action", json!("SELECT 1"));
    set(&mut v, "comment", json!("dbine"));
    set(&mut v, "enabled", json!(false));
    let saved = s.scheduler_save("alert", None, &v).await.unwrap();
    assert_eq!(saved.as_deref(), Some(id(ALERT).as_str()));
    let shown = rows(
        s,
        &format!("SHOW ALERTS LIKE '{ALERT}' IN SCHEMA {DB}.PUBLIC"),
    )
    .await;
    assert_eq!(shown[0]["comment"], "dbine");
    assert_eq!(shown[0]["state"], "suspended");

    let form = s.scheduler_form("alert", Some(&id(ALERT))).await.unwrap();
    let script = s
        .scheduler_script("alert", Some(&id(ALERT)), &values_of(&form))
        .await
        .unwrap();
    assert!(script.contains("No hay cambios"), "{script}");
    let mut v = values_of(&form);
    set(&mut v, "every", json!(120));
    set(&mut v, "comment", json!("changed"));
    set(&mut v, "condition", json!("SELECT 2 WHERE FALSE"));
    set(&mut v, "action", json!("SELECT 2"));
    s.scheduler_save("alert", Some(&id(ALERT)), &v)
        .await
        .unwrap();
    let shown = rows(
        s,
        &format!("SHOW ALERTS LIKE '{ALERT}' IN SCHEMA {DB}.PUBLIC"),
    )
    .await;
    assert_eq!(shown[0]["comment"], "changed");
    assert!(shown[0]["schedule"].starts_with("120"), "{shown:?}");
    assert!(shown[0]["condition"].contains("SELECT 2"), "{shown:?}");
    // Scheduled can't become on new data.
    set(&mut v, "schedule", json!("new_data"));
    assert!(s
        .scheduler_save("alert", Some(&id(ALERT)), &v)
        .await
        .is_err());

    s.scheduler_delete("alert", &id(ALERT)).await.unwrap();
    assert!(rows(
        s,
        &format!("SHOW ALERTS LIKE '{ALERT}' IN SCHEMA {DB}.PUBLIC")
    )
    .await
    .is_empty());
}

async fn task_graphs(s: &mut Box<dyn Session>, wh: &str) {
    let info = s.scheduler_info().await.unwrap();
    if !info.kinds.iter().any(|k| k.key == "task_graph") {
        eprintln!("task graph objects not available here: {:?}", info.notes);
        return;
    }
    run(
        s,
        &format!("CREATE TASK {DB}.PUBLIC.{G_ROOT} WAREHOUSE = {wh} AS SELECT 1"),
    )
    .await
    .unwrap();
    let form = s.scheduler_form("task_graph", None).await.unwrap();
    let mut v = values_of(&form);
    set(&mut v, "schema", json!(schema()));
    set(&mut v, "name", json!(GRAPH));
    set(&mut v, "schedule", json!("none"));
    set(&mut v, "root", json!(id(G_ROOT)));
    set(&mut v, "comment", json!("dbine"));
    set(&mut v, "d_warehouse", json!(wh));
    set(&mut v, "enabled", json!(false));
    set(&mut v, "commit", json!(true));
    // The kind is offered: the account has them, and a new one saves.
    let saved = s
        .scheduler_save("task_graph", None, &v)
        .await
        .unwrap_or_else(|e| panic!("task graphs are offered but a new one failed: {e}"));
    assert_eq!(saved.as_deref(), Some(id(GRAPH).as_str()));
    let items = s.scheduler_items("task_graph").await.unwrap();
    assert!(items.items.iter().any(|i| i.id == id(GRAPH)), "{items:?}");
    let form = s
        .scheduler_form("task_graph", Some(&id(GRAPH)))
        .await
        .unwrap();
    assert_eq!(form.values["root"], json!(id(G_ROOT)));
    let mut v = values_of(&form);
    set(&mut v, "comment", json!("changed"));
    set(&mut v, "d_log_level", json!("INFO"));
    eprintln!(
        "{}",
        s.scheduler_script("task_graph", Some(&id(GRAPH)), &v)
            .await
            .unwrap()
    );
    s.scheduler_save("task_graph", Some(&id(GRAPH)), &v)
        .await
        .unwrap();
    let shown = rows(
        s,
        &format!("SHOW TASK GRAPHS LIKE '{GRAPH}' IN SCHEMA {DB}.PUBLIC"),
    )
    .await;
    assert_eq!(shown[0]["comment"], "changed");
    s.scheduler_delete("task_graph", &id(GRAPH)).await.unwrap();
    assert!(rows(
        s,
        &format!("SHOW TASK GRAPHS LIKE '{GRAPH}' IN SCHEMA {DB}.PUBLIC")
    )
    .await
    .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tasks_alerts_and_task_graphs_are_designed() {
    let Some(cfg) = config() else {
        eprintln!("sin DBINE_TEST_SNOWFLAKE_*: se omite");
        return;
    };
    let d = dbine_driver_snowflake::drivers().remove(0);
    assert!(d.capabilities().scheduler);
    let boot = ConnectionConfig {
        database: String::new(),
        ..cfg.clone()
    };
    let mut admin = d.connect(&boot, None).await.expect("connect");
    let wh = cfg.option("warehouse").unwrap().to_string();
    run(&mut admin, &format!("CREATE DATABASE IF NOT EXISTS {DB}"))
        .await
        .unwrap();
    let body = {
        let (d, cfg) = (d.clone(), cfg.clone());
        tokio::spawn(async move {
            let mut s = d.connect(&cfg, None).await.expect("connect");
            tasks(&mut s, &wh).await;
            alerts(&mut s, &wh).await;
            task_graphs(&mut s, &wh).await;
            assert!(s.list_databases().await.unwrap().iter().any(|x| x == DB));
        })
    };
    let outcome = body.await;
    // Cleanup before reporting: the database goes with its tasks, alerts
    // and graphs.
    run(&mut admin, &format!("DROP DATABASE IF EXISTS {DB}"))
        .await
        .unwrap();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

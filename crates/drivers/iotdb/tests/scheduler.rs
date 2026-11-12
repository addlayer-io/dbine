//! IoTDB's scheduler managed through the scheduler contract (continuous
//! queries, pipes and pipe plugins), against real servers: IoTDB 1.x
//! (`DBINE_TEST_IOTDB_URL`) and 2.x (`DBINE_TEST_IOTDB2_URL`), user root /
//! root, the `apache/iotdb` standalone images (the plugin test loads a
//! built-in class from the image's `/iotdb/lib/node-commons-<version>.jar`).
//! Each is skipped without its variable:
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 DBINE_TEST_IOTDB2_URL=http://localhost:27150 \
//!   cargo test -p dbine-driver-iotdb --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! Every object a test makes is named `dbine_test_sched_*` and is dropped at
//! the end, whether the test passed or not.

use dbine_driver::{ConnectionConfig, FormValues, ObjectForm, QueryOutcome, Session};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const DB: &str = "root.dbine_test_sched";
const CQ: &str = "dbine_test_sched_cq";
const CQ2: &str = "dbine_test_sched_cq 2";
const PIPE: &str = "dbine_test_sched_pipe";
const PIPE2: &str = "dbine_test_sched_pipe2";
const PLUGIN: &str = "DBINE_TEST_SCHED_PLUGIN";
const SECRET: &str = "dbine_test_sched_secret";

fn cfg(env: &str) -> Option<ConnectionConfig> {
    Some(ConnectionConfig {
        driver: "iotdb".into(),
        host: std::env::var(env).ok()?,
        username: Some("root".into()),
        password: Some("root".into()),
        ..Default::default()
    })
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_iotdb::drivers()[0]
        .connect(c, None)
        .await
        .expect("connect")
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await?;
    match out.error.clone() {
        Some(e) => Err(dbine_driver::Error::Query(e)),
        None => Ok(out),
    }
}

/// The last result's rows.
async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    exec(s, sql)
        .await
        .expect(sql)
        .results
        .iter()
        .rfind(|r| !r.columns.is_empty())
        .map(|r| r.rows.clone())
        .unwrap_or_default()
}

/// Drops everything named `dbine_test_sched_*`.
async fn cleanup(c: &ConnectionConfig) {
    let mut s = open(c).await;
    for r in rows(&mut s, "SHOW CQS").await {
        if let Some(id) = r[0].as_str().filter(|n| n.starts_with("dbine_test_sched")) {
            let _ = exec(&mut s, &format!("DROP CQ `{}`", id.replace('`', "``"))).await;
        }
    }
    for r in rows(&mut s, "SHOW PIPES").await {
        if let Some(n) = r[0].as_str().filter(|n| n.starts_with("dbine_test_sched")) {
            let _ = exec(&mut s, &format!("DROP PIPE `{}`", n.replace('`', "``"))).await;
        }
    }
    for r in rows(&mut s, "SHOW PIPEPLUGINS").await {
        if let Some(n) = r[0].as_str().filter(|n| n.starts_with("DBINE_TEST_SCHED")) {
            let _ = exec(&mut s, &format!("DROP PIPEPLUGIN `{n}`")).await;
        }
    }
    let _ = exec(&mut s, &format!("DROP DATABASE {DB}")).await;
}

/// How many test objects are left.
async fn left(c: &ConnectionConfig) -> usize {
    let mut s = open(c).await;
    let mut n = 0;
    for (sql, prefix) in [
        ("SHOW CQS", "dbine_test_sched"),
        ("SHOW PIPES", "dbine_test_sched"),
        ("SHOW PIPEPLUGINS", "DBINE_TEST_SCHED"),
    ] {
        n += rows(&mut s, sql)
            .await
            .iter()
            .filter(|r| r[0].as_str().is_some_and(|x| x.starts_with(prefix)))
            .count();
    }
    n
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

fn row(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

async fn save(s: &mut Box<dyn Session>, kind: &str, id: Option<&str>, v: &FormValues) -> String {
    let script = s.scheduler_script(kind, id, v).await.expect("script");
    eprintln!("--- {kind} {id:?}\n{script}");
    assert!(
        !script.contains(SECRET),
        "a credential in the script: {script}"
    );
    s.scheduler_save(kind, id, v)
        .await
        .unwrap_or_else(|e| panic!("save {kind}: {e:?}\n{script}"))
        .expect("an id")
}

/// `SHOW PIPES` row of the pipe: state and the three clauses.
async fn pipe_row(s: &mut Box<dyn Session>, name: &str) -> Option<Vec<String>> {
    rows(s, "SHOW PIPES").await.into_iter().find_map(|r| {
        (r[0].as_str() == Some(name)).then(|| {
            [2, 3, 4, 5]
                .iter()
                .map(|i| r[*i].as_str().unwrap_or("").to_string())
                .collect()
        })
    })
}

async fn cq_statement(s: &mut Box<dyn Session>, id: &str) -> Option<String> {
    rows(s, "SHOW CQS")
        .await
        .into_iter()
        .find(|r| r[0].as_str() == Some(id))
        .and_then(|r| r[1].as_str().map(str::to_string))
}

fn list_mut<'a>(v: &'a mut FormValues, list: &str) -> &'a mut Vec<BTreeMap<String, Value>> {
    v.lists.entry(list.into()).or_default()
}

async fn body(c: ConnectionConfig) {
    let d = &dbine_driver_iotdb::drivers()[0];
    assert!(d.capabilities().scheduler);
    let mut s = open(&c).await;
    let version = rows(&mut s, "SHOW VERSION").await[0][0]
        .as_str()
        .unwrap()
        .to_string();
    let v2 = version.starts_with('2');
    exec(&mut s, &format!("CREATE DATABASE {DB}"))
        .await
        .expect("database");
    exec(
        &mut s,
        &format!("CREATE TIMESERIES {DB}.d1.s1 WITH DATATYPE=INT64, ENCODING=PLAIN"),
    )
    .await
    .expect("series");

    // What the scheduler has.
    let info = s.scheduler_info().await.expect("info");
    let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "pipe", "pipe_plugin"]);
    assert!(info
        .kinds
        .iter()
        .all(|k| k.can_create && k.can_edit && k.can_delete));
    assert!(!info.logs);

    // ---------------------------------------------------- continuous query
    let form = s.scheduler_form("job", None).await.expect("form");
    let mut v = values_of(&form);
    set(&mut v, "name", json!(CQ));
    set(&mut v, "every", json!("30s"));
    set(&mut v, "range_start", json!("1m"));
    set(&mut v, "timeout_policy", json!("discard"));
    set(
        &mut v,
        "query",
        json!(format!(
            "SELECT count(s1) INTO {DB}.agg(c) FROM {DB}.d1 GROUP BY(30s)"
        )),
    );
    let id = save(&mut s, "job", None, &v).await;
    assert_eq!(id, format!("cq:{CQ}"));
    let st = cq_statement(&mut s, CQ).await.expect("created");
    assert!(
        st.contains("EVERY 30s") && st.contains("RANGE 1m") && st.contains("DISCARD"),
        "{st}"
    );
    // Listed under jobs with the jobs list's id.
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));
    let items = s.scheduler_items("job").await.expect("items");
    assert!(items
        .items
        .iter()
        .any(|i| i.id == id && i.enabled == Some(true)));
    // Read back.
    let form = s.scheduler_form("job", Some(&id)).await.expect("form");
    assert_eq!(form.values["every"], json!("30s"));
    assert_eq!(form.values["range_start"], json!("1m"));
    assert_eq!(form.values["timeout_policy"], json!("discard"));
    // Unchanged: nothing to run.
    let mut v = values_of(&form);
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(script.contains("no hay cambios"), "{script}");
    // A new schedule: dropped and created again.
    set(&mut v, "every", json!("1m"));
    set(&mut v, "boundary", json!("2024-01-01T00:00:00"));
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(
        script.contains("DROP CQ") && script.contains("EVERY 1m BOUNDARY 2024-01-01T00:00:00"),
        "{script}"
    );
    save(&mut s, "job", Some(&id), &v).await;
    let st = cq_statement(&mut s, CQ).await.expect("still there");
    assert!(
        st.contains("EVERY 1m") && st.contains("BOUNDARY 2024-01-01T00:00:00"),
        "{st}"
    );
    // A statement the server refuses: the old one is put back.
    let before = st.clone();
    let mut bad = v.clone();
    set(
        &mut bad,
        "query",
        json!(format!(
            "SELECT count(s1) INTO {DB}.agg(c) FROM {DB}.d1 GROUP BY(30s) HAVING"
        )),
    );
    let e = s.scheduler_save("job", Some(&id), &bad).await.unwrap_err();
    assert!(e.to_string().contains("como estaba"), "{e}");
    assert_eq!(
        cq_statement(&mut s, CQ).await.as_deref(),
        Some(before.as_str())
    );
    // Renamed (a name with a space and a backquote-free quote).
    set(&mut v, "name", json!(CQ2));
    let id2 = save(&mut s, "job", Some(&id), &v).await;
    assert_eq!(id2, format!("cq:{CQ2}"));
    assert!(cq_statement(&mut s, CQ).await.is_none());
    assert!(cq_statement(&mut s, CQ2).await.is_some());
    // Values the driver refuses before the server sees them.
    let mut v_bad = v.clone();
    set(&mut v_bad, "every", json!("1m; DROP DATABASE root.x"));
    assert!(s.scheduler_script("job", Some(&id2), &v_bad).await.is_err());
    let mut v_bad = v.clone();
    set(
        &mut v_bad,
        "query",
        json!(format!(
            "SELECT 1 INTO {DB}.x(c) FROM {DB}.d1; DROP DATABASE {DB}"
        )),
    );
    assert!(s.scheduler_save("job", Some(&id2), &v_bad).await.is_err());
    assert!(cq_statement(&mut s, CQ2).await.is_some());
    // Deleted.
    let script = s.scheduler_delete_script("job", &id2).await.unwrap();
    assert_eq!(
        script.lines().last(),
        Some("DROP CQ `dbine_test_sched_cq 2`;")
    );
    s.scheduler_delete("job", &id2).await.expect("delete");
    assert!(cq_statement(&mut s, CQ2).await.is_none());
    for bad in [
        "cq:nope",
        "cq:dbine_test_sched_cq 2`; DROP DATABASE root.x",
        "pipe:",
        "x",
    ] {
        assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad}");
        assert!(s.scheduler_delete("job", bad).await.is_err(), "{bad}");
    }

    // ------------------------------------------------------------------ pipe
    let form = s.scheduler_form("pipe", None).await.expect("form");
    let sinks: Vec<&str> = form.choices["pipe_plugin:sink"]
        .iter()
        .map(|(v, _)| v.as_str())
        .collect();
    assert!(sinks.contains(&"do-nothing-sink"), "{sinks:?}");
    let mut v = values_of(&form);
    set(&mut v, "name", json!(PIPE));
    set(&mut v, "enabled", json!(false));
    set(&mut v, "source", json!("iotdb-source"));
    list_mut(&mut v, "source_attrs").push(row(&[
        ("key", json!("source.pattern")),
        ("value", json!(DB)),
    ]));
    list_mut(&mut v, "source_attrs").push(row(&[
        ("key", json!("source.history.enable")),
        ("value", json!("false")),
    ]));
    set(&mut v, "sink", json!("do-nothing-sink"));
    list_mut(&mut v, "sink_attrs").push(row(&[
        ("key", json!("sink.note")),
        ("value", json!("it's")),
    ]));
    list_mut(&mut v, "secrets").push(row(&[
        ("clause", json!("sink")),
        ("key", json!("sink.password")),
        ("value", json!(SECRET)),
    ]));
    let pid = save(&mut s, "pipe", None, &v).await;
    assert_eq!(pid, format!("pipe:{PIPE}"));
    let r = pipe_row(&mut s, PIPE).await.expect("created");
    assert_eq!(r[0], "STOPPED");
    assert!(
        r[1].contains(&format!("source.pattern={DB}")) && r[3].contains("sink.note=it's"),
        "{r:?}"
    );
    // The form, also through the jobs kind, never carries the password.
    let form = s.scheduler_form("pipe", Some(&pid)).await.expect("form");
    let as_job = s
        .scheduler_form("job", Some(&pid))
        .await
        .expect("form as a job");
    assert_eq!(form, as_job);
    assert!(!serde_json::to_string(&form).unwrap().contains(SECRET));
    assert_eq!(form.values["enabled"], json!(false));
    let mut v = values_of(&form);
    assert!(v.lists["secrets"]
        .iter()
        .any(|r| r["key"] == json!("sink.password") && r["value"].is_null()));
    let script = s.scheduler_script("pipe", Some(&pid), &v).await.unwrap();
    assert!(script.contains("no hay cambios"), "{script}");
    // An attribute added, and started: ALTER … MODIFY SINK, START PIPE.
    list_mut(&mut v, "sink_attrs").push(row(&[
        ("key", json!("sink.batch.enable")),
        ("value", json!("false")),
    ]));
    set(&mut v, "enabled", json!(true));
    let script = s.scheduler_script("pipe", Some(&pid), &v).await.unwrap();
    assert!(
        script.contains("MODIFY SINK ('sink.batch.enable'='false')")
            && script.contains("START PIPE"),
        "{script}"
    );
    save(&mut s, "pipe", Some(&pid), &v).await;
    let r = pipe_row(&mut s, PIPE).await.unwrap();
    assert_eq!(r[0], "RUNNING");
    assert!(r[3].contains("sink.batch.enable=false"), "{r:?}");
    assert!(r[3].contains("sink.password="), "the password stays: {r:?}");
    // An attribute taken away: REPLACE SINK. 2.x hides the password, so it
    // has to be typed again; 1.x sends its own back.
    let form = s.scheduler_form("pipe", Some(&pid)).await.unwrap();
    let mut v = values_of(&form);
    list_mut(&mut v, "sink_attrs").retain(|r| r["key"] != json!("sink.note"));
    let script = s.scheduler_script("pipe", Some(&pid), &v).await.unwrap();
    assert!(script.contains("REPLACE SINK"), "{script}");
    if v2 {
        let e = s.scheduler_save("pipe", Some(&pid), &v).await.unwrap_err();
        assert!(e.to_string().contains("no devuelve"), "{e}");
        for r in list_mut(&mut v, "secrets") {
            if r["key"] == json!("sink.password") {
                r.insert("value".into(), json!(SECRET));
            }
        }
    }
    save(&mut s, "pipe", Some(&pid), &v).await;
    let r = pipe_row(&mut s, PIPE).await.unwrap();
    assert!(
        !r[3].contains("sink.note") && r[3].contains("sink.password="),
        "{r:?}"
    );
    if !v2 {
        assert!(
            r[3].contains(&format!("sink.password={SECRET}")),
            "1.x kept it: {r:?}"
        );
    }
    // The source: in place on 2.x, dropped and created again on 1.x.
    let form = s.scheduler_form("pipe", Some(&pid)).await.unwrap();
    let mut v = values_of(&form);
    for r in list_mut(&mut v, "source_attrs") {
        if r["key"] == json!("source.pattern") {
            r.insert("value".into(), json!(format!("{DB}.d1")));
        }
    }
    let script = s.scheduler_script("pipe", Some(&pid), &v).await.unwrap();
    if v2 {
        assert!(
            script.contains("MODIFY SOURCE") && !script.contains("DROP PIPE"),
            "{script}"
        );
    } else {
        assert!(
            script.contains("DROP PIPE") && script.contains("CREATE PIPE"),
            "{script}"
        );
    }
    save(&mut s, "pipe", Some(&pid), &v).await;
    let r = pipe_row(&mut s, PIPE).await.unwrap();
    assert!(r[1].contains(&format!("source.pattern={DB}.d1")), "{r:?}");
    assert!(r[3].contains("sink.password="), "{r:?}");
    // Renamed: created under the new name, the old one dropped.
    let form = s.scheduler_form("pipe", Some(&pid)).await.unwrap();
    let mut v = values_of(&form);
    set(&mut v, "name", json!(PIPE2));
    for r in list_mut(&mut v, "secrets") {
        // 2.x adds the creator's login to the source, and checks it.
        let pw = if r["key"] == json!("source.password") {
            "root"
        } else {
            SECRET
        };
        r.insert("value".into(), json!(pw));
    }
    let pid2 = save(&mut s, "pipe", Some(&pid), &v).await;
    assert_eq!(pid2, format!("pipe:{PIPE2}"));
    assert!(pipe_row(&mut s, PIPE).await.is_none());
    assert!(pipe_row(&mut s, PIPE2).await.is_some());
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == pid2));
    // Deleted.
    let script = s.scheduler_delete_script("pipe", &pid2).await.unwrap();
    assert!(
        script.ends_with("DROP PIPE `dbine_test_sched_pipe2`;\n"),
        "{script}"
    );
    s.scheduler_delete("pipe", &pid2).await.expect("delete");
    assert!(pipe_row(&mut s, PIPE2).await.is_none());

    // ------------------------------------------------------------ pipe plugin
    let items = s.scheduler_items("pipe_plugin").await.expect("plugins");
    let builtin = items
        .items
        .iter()
        .find(|i| i.name == "DO-NOTHING-PROCESSOR")
        .expect("a built-in processor");
    assert!(builtin.system);
    let class = builtin.description.clone().unwrap();
    let f = s
        .scheduler_form("pipe_plugin", Some(&builtin.id))
        .await
        .unwrap();
    assert!(f.read_only);
    assert!(s
        .scheduler_delete("pipe_plugin", &builtin.id)
        .await
        .is_err());
    let form = s.scheduler_form("pipe_plugin", None).await.unwrap();
    let mut v = values_of(&form);
    set(&mut v, "name", json!(PLUGIN.to_ascii_lowercase()));
    set(&mut v, "class_name", json!(class));
    set(
        &mut v,
        "uri",
        json!(format!("file:///iotdb/lib/node-commons-{version}.jar")),
    );
    let plid = save(&mut s, "pipe_plugin", None, &v).await;
    assert_eq!(plid, format!("plugin:{PLUGIN}"));
    let items = s.scheduler_items("pipe_plugin").await.unwrap();
    assert!(items.items.iter().any(|i| i.id == plid && !i.system));
    let form = s.scheduler_form("pipe_plugin", Some(&plid)).await.unwrap();
    let mut v = values_of(&form);
    let script = s
        .scheduler_script("pipe_plugin", Some(&plid), &v)
        .await
        .unwrap();
    assert!(script.contains("no hay cambios"), "{script}");
    // A pipe can use it by name.
    let pf = s.scheduler_form("pipe", None).await.unwrap();
    assert!(pf.choices["pipe_plugin:processor"]
        .iter()
        .any(|(x, _)| x == &PLUGIN.to_ascii_lowercase()));
    // Loaded again from its URI.
    set(
        &mut v,
        "uri",
        json!(format!("file:///iotdb/lib/node-commons-{version}.jar")),
    );
    let script = s
        .scheduler_script("pipe_plugin", Some(&plid), &v)
        .await
        .unwrap();
    assert!(
        script.contains("DROP PIPEPLUGIN") && script.contains("CREATE PIPEPLUGIN"),
        "{script}"
    );
    save(&mut s, "pipe_plugin", Some(&plid), &v).await;
    s.scheduler_delete("pipe_plugin", &plid)
        .await
        .expect("delete");
    let items = s.scheduler_items("pipe_plugin").await.unwrap();
    assert!(!items.items.iter().any(|i| i.id == plid));
}

async fn run(env: &str) {
    let Some(c) = cfg(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    cleanup(&c).await;
    let r = tokio::spawn(body(c.clone())).await;
    cleanup(&c).await;
    let n = left(&c).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
    assert_eq!(n, 0, "objects left behind");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn the_scheduler_on_iotdb_1() {
    run("DBINE_TEST_IOTDB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn the_scheduler_on_iotdb_2() {
    run("DBINE_TEST_IOTDB2_URL").await;
}

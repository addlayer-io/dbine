//! TDengine's streams managed through the scheduler contract, against real
//! servers through taosAdapter's REST port: the old stream engine (up to
//! 3.3.6, `DBINE_TEST_TDENGINE_URL`) and the new one (3.3.7 on,
//! `DBINE_TEST_TDENGINE_NEW_URL`, image `tdengine/tsdb`). Each is skipped
//! without its variable:
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//! DBINE_TEST_TDENGINE_NEW_URL=http://localhost:25741 \
//!   cargo test -p dbine-driver-tdengine --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! Every object a test makes is named `dbine_test_sched*` and is dropped at
//! the end, whether the test passed or not.

use dbine_driver::{ConnectionConfig, FormValues, ObjectForm, QueryOutcome, Session};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const DB: &str = "dbine_test_sched";

fn cfg(env: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    })
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_tdengine::drivers()[0]
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

/// The test's streams: (name, database or null, status, sql).
async fn streams(s: &mut Box<dyn Session>, new_engine: bool) -> Vec<(String, String, String)> {
    let sql = if new_engine {
        "SELECT stream_name, status, sql FROM information_schema.ins_streams WHERE db_name = 'dbine_test_sched'"
    } else {
        "SELECT stream_name, status, sql FROM information_schema.ins_streams WHERE stream_name LIKE 'dbine_test_sched%'"
    };
    rows(s, sql)
        .await
        .into_iter()
        .map(|r| {
            let t = |i: usize| r[i].as_str().unwrap_or("").to_string();
            (t(0), t(1), t(2))
        })
        .collect()
}

async fn new_engine(s: &mut Box<dyn Session>) -> bool {
    exec(s, "SELECT * FROM information_schema.ins_streams")
        .await
        .unwrap()
        .results
        .iter()
        .any(|r| r.columns.iter().any(|c| c.name == "db_name"))
}

async fn cleanup(c: &ConnectionConfig) {
    let mut s = open(c).await;
    let new = new_engine(&mut s).await;
    for (name, _, _) in streams(&mut s, new).await {
        let target = if new {
            format!("`{DB}`.`{name}`")
        } else {
            format!("`{name}`")
        };
        let _ = exec(&mut s, &format!("DROP STREAM IF EXISTS {target}")).await;
    }
    let _ = exec(&mut s, &format!("DROP DATABASE IF EXISTS {DB}")).await;
}

async fn left(c: &ConnectionConfig) -> usize {
    let mut s = open(c).await;
    let new = new_engine(&mut s).await;
    let n = streams(&mut s, new).await.len();
    n + rows(
        &mut s,
        "SELECT name FROM information_schema.ins_databases WHERE name = 'dbine_test_sched'",
    )
    .await
    .len()
}

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
    s.scheduler_save("job", id, v)
        .await
        .unwrap_or_else(|e| panic!("save: {e:?}\n{script}"))
        .expect("an id")
}

async fn status(s: &mut Box<dyn Session>, new: bool, name: &str) -> Option<(String, String)> {
    streams(s, new)
        .await
        .into_iter()
        .find(|(n, _, _)| n == name)
        .map(|(_, st, sql)| (st.to_ascii_lowercase(), sql))
}

async fn setup(s: &mut Box<dyn Session>) {
    exec(s, &format!("CREATE DATABASE IF NOT EXISTS {DB}"))
        .await
        .expect("database");
    exec(
        s,
        &format!(
            "CREATE STABLE IF NOT EXISTS {DB}.meters (ts TIMESTAMP, v INT) TAGS (loc VARCHAR(16))"
        ),
    )
    .await
    .expect("stable");
    exec(
        s,
        &format!("CREATE TABLE IF NOT EXISTS {DB}.d1 USING {DB}.meters TAGS ('a')"),
    )
    .await
    .expect("table");
}

/// Shared by both engines: what the info, the ids and the refusals say.
async fn common(s: &mut Box<dyn Session>, id: &str) {
    let info = s.scheduler_info().await.expect("info");
    assert_eq!(info.kinds.len(), 1);
    assert_eq!(info.kinds[0].key, "job");
    assert!(info.kinds[0].can_create && info.kinds[0].can_edit && info.kinds[0].can_delete);
    assert!(!info.logs);
    let items = s.scheduler_items("job").await.expect("items");
    assert!(items.items.iter().any(|i| i.id == id), "{items:?}");
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));
    for bad in [
        "nope",
        "dbine_test_sched_s1`; DROP DATABASE dbine_test_sched",
        "",
    ] {
        assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad}");
        assert!(s.scheduler_delete("job", bad).await.is_err(), "{bad}");
    }
    assert!(s.scheduler_items("pipe").await.is_err());
    assert!(s.run_job_from(id, "x").await.is_err());
    assert!(s.clear_job_history(id).await.is_err());
}

async fn old_body(c: ConnectionConfig) {
    let mut s = open(&c).await;
    assert!(!new_engine(&mut s).await);
    setup(&mut s).await;
    let name = "dbine_test_sched_s1";

    // A new stream, paused at once.
    let form = s.scheduler_form("job", None).await.expect("form");
    assert!(form.choices["database"].iter().any(|(d, _)| d == DB));
    let mut v = values_of(&form);
    set(&mut v, "database", json!(DB));
    set(&mut v, "name", json!(name));
    set(&mut v, "enabled", json!(false));
    set(&mut v, "trigger", json!("max_delay"));
    set(&mut v, "max_delay", json!("5s"));
    set(&mut v, "watermark", json!("10s"));
    set(&mut v, "ignore_expired", json!("0"));
    set(&mut v, "delete_mark", json!("1d"));
    set(&mut v, "fill_history", json!("1"));
    set(&mut v, "ignore_update", json!("1"));
    set(&mut v, "into", json!("agg1"));
    set(&mut v, "columns", json!("wstart, c"));
    set(&mut v, "tags", json!("loc VARCHAR(16)"));
    set(&mut v, "subtable", json!("CONCAT('agg_', loc)"));
    set(
        &mut v,
        "query",
        json!("SELECT _wstart, count(*) FROM meters PARTITION BY loc INTERVAL(10s)"),
    );
    v.lists.insert(
        "notify_urls".into(),
        vec![BTreeMap::from([(
            "url".to_string(),
            json!("ws://localhost:12345/notify"),
        )])],
    );
    set(&mut v, "notify_events", json!(["window_close"]));
    set(&mut v, "on_failure", json!("pause"));
    let id = save(&mut s, None, &v).await;
    assert_eq!(id, name);
    let (st, sql) = status(&mut s, false, name).await.expect("created");
    assert_eq!(st, "paused");
    assert!(
        sql.contains("trigger max_delay 5s")
            && sql.contains("notify('ws://localhost:12345/notify')"),
        "{sql}"
    );
    common(&mut s, &id).await;

    // Read back: as it came, nothing to do.
    let form = s.scheduler_form("job", Some(&id)).await.expect("form");
    assert_eq!(form.values["database"], json!(DB));
    assert_eq!(form.values["max_delay"], json!("5s"));
    assert_eq!(form.values["enabled"], json!(false));
    let mut v = values_of(&form);
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(script.contains("no hay cambios"), "{script}");
    // Resumed, skipping what came meanwhile.
    set(&mut v, "enabled", json!(true));
    set(&mut v, "resume_ignore_untreated", json!(true));
    let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
    assert!(
        script.contains("RESUME STREAM IGNORE UNTREATED"),
        "{script}"
    );
    save(&mut s, Some(&id), &v).await;
    assert_ne!(status(&mut s, false, name).await.unwrap().0, "paused");
    // Changed: dropped and created again.
    set(&mut v, "watermark", json!("20s"));
    set(&mut v, "resume_ignore_untreated", json!(false));
    save(&mut s, Some(&id), &v).await;
    let (_, sql) = status(&mut s, false, name).await.unwrap();
    assert!(sql.contains("watermark 20s"), "{sql}");
    // Refused by the server: the old one is put back.
    let mut bad = v.clone();
    set(
        &mut bad,
        "query",
        json!("SELECT _wstart, count(*) FROM dbine_test_sched_nope INTERVAL(10s)"),
    );
    let e = s.scheduler_save("job", Some(&id), &bad).await.unwrap_err();
    assert!(e.to_string().contains("como estaba"), "{e}");
    let (_, again) = status(&mut s, false, name).await.expect("put back");
    assert_eq!(again, sql);
    // Refused before the server sees it.
    let mut bad = v.clone();
    set(
        &mut bad,
        "tags",
        json!("loc VARCHAR(16)) AS SELECT 1; DROP DATABASE dbine_test_sched"),
    );
    assert!(s.scheduler_save("job", Some(&id), &bad).await.is_err());
    // Renamed.
    let name2 = "dbine_test_sched_s2";
    set(&mut v, "name", json!(name2));
    let id2 = save(&mut s, Some(&id), &v).await;
    assert_eq!(id2, name2);
    assert!(status(&mut s, false, name).await.is_none());
    // Deleted.
    let script = s.scheduler_delete_script("job", &id2).await.unwrap();
    assert!(
        script.ends_with("DROP STREAM `dbine_test_sched_s2`;\n"),
        "{script}"
    );
    s.scheduler_delete("job", &id2).await.expect("delete");
    assert!(status(&mut s, false, name2).await.is_none());
}

async fn new_body(c: ConnectionConfig) {
    let mut s = open(&c).await;
    assert!(new_engine(&mut s).await);
    setup(&mut s).await;
    let name = "dbine_test_sched_s1";

    // A new stream, stopped at once.
    let form = s.scheduler_form("job", None).await.expect("form");
    let mut v = values_of(&form);
    assert_eq!(v.values["trigger"], json!("sliding"));
    set(&mut v, "database", json!(DB));
    set(&mut v, "name", json!(name));
    set(&mut v, "enabled", json!(false));
    set(&mut v, "interval", json!("10s"));
    set(&mut v, "sliding", json!("10s"));
    set(&mut v, "from_table", json!("meters"));
    set(&mut v, "partition_by", json!("tbname, loc"));
    set(&mut v, "watermark", json!("5s"));
    set(&mut v, "expired_time", json!("1d"));
    set(&mut v, "max_delay", json!("3s"));
    set(&mut v, "fill", json!("fill_history"));
    set(&mut v, "fill_start", json!("2026-01-01 00:00:00"));
    set(&mut v, "timezone", json!("UTC"));
    set(&mut v, "into", json!("out1"));
    set(&mut v, "subtable", json!("CONCAT('o_', `loc`)"));
    set(&mut v, "columns", json!("ts, c"));
    set(&mut v, "tags", json!("loc VARCHAR(16) AS loc"));
    set(
        &mut v,
        "query",
        json!("SELECT _twstart, count(*) FROM %%trows"),
    );
    v.lists.insert(
        "notify_urls".into(),
        vec![BTreeMap::from([(
            "url".to_string(),
            json!("ws://localhost:12345/x"),
        )])],
    );
    set(
        &mut v,
        "notify_events",
        json!(["window_open", "window_close"]),
    );
    set(&mut v, "notify_history", json!(true));
    let id = save(&mut s, None, &v).await;
    assert_eq!(id, format!("{DB}.{name}"));
    let (st, sql) = status(&mut s, true, name).await.expect("created");
    assert_eq!(st, "stopped");
    assert!(
        sql.contains("interval(10s) sliding(10s)") && sql.contains("timezone = 'UTC'"),
        "{sql}"
    );
    common(&mut s, &id).await;

    // Read back.
    let form = s.scheduler_form("job", Some(&id)).await.expect("form");
    assert_eq!(form.values["timezone"], json!("UTC"));
    assert_eq!(form.values["partition_by"], json!("tbname, loc"));
    assert_eq!(form.values["fill_start"], json!("2026-01-01 00:00:00"));
    let mut v = values_of(&form);
    assert!(s
        .scheduler_script("job", Some(&id), &v)
        .await
        .unwrap()
        .contains("no hay cambios"));
    // Started.
    set(&mut v, "enabled", json!(true));
    save(&mut s, Some(&id), &v).await;
    assert_ne!(status(&mut s, true, name).await.unwrap().0, "stopped");
    // An option: dropped and created again, into the table it made.
    set(&mut v, "watermark", json!("6s"));
    save(&mut s, Some(&id), &v).await;
    let (_, sql) = status(&mut s, true, name).await.unwrap();
    assert!(
        sql.contains("watermark(6s)") && sql.contains("tags (loc varchar(16) as loc)"),
        "{sql}"
    );
    // Another trigger: dropped and created again.
    set(&mut v, "trigger", json!("period"));
    set(&mut v, "period", json!("1m"));
    set(&mut v, "from_table", json!(""));
    set(&mut v, "partition_by", json!(""));
    set(&mut v, "subtable", json!(""));
    set(&mut v, "tags", json!(""));
    set(&mut v, "fill", json!(""));
    set(&mut v, "fill_start", json!(""));
    set(
        &mut v,
        "query",
        json!("SELECT cast(_tlocaltime/1000000 as timestamp) ts, count(*) c FROM meters"),
    );
    set(&mut v, "into", json!("out2"));
    v.lists.insert("notify_urls".into(), Vec::new());
    set(&mut v, "notify_events", json!([]));
    set(&mut v, "notify_history", json!(false));
    save(&mut s, Some(&id), &v).await;
    let (_, sql) = status(&mut s, true, name).await.unwrap();
    assert!(sql.contains("period(1m)"), "{sql}");
    // Refused by the server: the old one is put back.
    let mut bad = v.clone();
    set(
        &mut bad,
        "query",
        json!("SELECT now ts, count(*) c FROM dbine_test_sched_nope"),
    );
    let e = s.scheduler_save("job", Some(&id), &bad).await.unwrap_err();
    assert!(e.to_string().contains("como estaba"), "{e}");
    assert_eq!(status(&mut s, true, name).await.expect("put back").1, sql);
    // Renamed.
    let name2 = "dbine_test_sched s2";
    set(&mut v, "name", json!(name2));
    let id2 = save(&mut s, Some(&id), &v).await;
    assert_eq!(id2, format!("{DB}.{name2}"));
    assert!(status(&mut s, true, name).await.is_none());
    assert!(status(&mut s, true, name2).await.is_some());
    // Deleted.
    s.scheduler_delete("job", &id2).await.expect("delete");
    assert!(status(&mut s, true, name2).await.is_none());
}

async fn run(env: &str, new: bool) {
    let Some(c) = cfg(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    cleanup(&c).await;
    let r = if new {
        tokio::spawn(new_body(c.clone())).await
    } else {
        tokio::spawn(old_body(c.clone())).await
    };
    cleanup(&c).await;
    let n = left(&c).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
    assert_eq!(n, 0, "objects left behind");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn streams_on_the_old_engine() {
    run("DBINE_TEST_TDENGINE_URL", false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn streams_on_the_new_engine() {
    run("DBINE_TEST_TDENGINE_NEW_URL", true).await;
}

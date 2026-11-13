//! The scheduler designer (persistent queries) against a real ksqlDB
//! (`DBINE_TEST_KSQLDB_URL`, see tests/integration.rs for the containers);
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_KSQLDB_URL=http://localhost:25188 \
//!   cargo test -p dbine-driver-ksqldb --test scheduler -- --ignored
//! ```
//!
//! Every stream, table and query the test makes is named
//! `dbine_test_sched_*` and is dropped at the end (topics included), also
//! when an assertion fails.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, QueryOutcome, Session};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const PREFIX: &str = "DBINE_TEST_SCHED_";
const SRC: &str = "DBINE_TEST_SCHED_SRC";
const INS: &str = "DBINE_TEST_SCHED_INS";
const CST: &str = "DBINE_TEST_SCHED_CST";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_KSQLDB_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "ksqldb".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        ..Default::default()
    })
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_ksqldb::drivers()[0]
        .connect(c, None)
        .await
        .expect("connect")
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await?;
    match out.error.clone() {
        Some(e) => Err(Error::Query(e)),
        None => Ok(out),
    }
}

/// Names of the streams or tables of a `SHOW STREAMS` / `SHOW TABLES`.
async fn names(s: &mut Box<dyn Session>, what: &str) -> Vec<String> {
    let out = exec(s, &format!("SHOW {what}")).await.expect("show");
    let Some(r) = out.results.iter().rfind(|r| !r.columns.is_empty()) else {
        return Vec::new();
    };
    let Some(col) = r.columns.iter().position(|c| c.name == "name") else {
        return Vec::new();
    };
    r.rows
        .iter()
        .map(|row| row[col].as_str().unwrap_or_default().to_string())
        .collect()
}

fn ours(name: &str) -> bool {
    name.to_ascii_uppercase().starts_with(PREFIX)
}

/// The test's Kafka topics still there: the ones a recreate leaves (the
/// topic stays when its stream is dropped) and the changelog ksqlDB keeps
/// of a source table's query after the table is dropped.
async fn topics_left(s: &mut Box<dyn Session>) -> Vec<String> {
    names(s, "ALL TOPICS")
        .await
        .into_iter()
        .filter(|t| t.to_ascii_uppercase().contains(PREFIX))
        .collect()
}

/// Deletes a leftover topic through ksqlDB: a stream over it, dropped
/// with its topic.
async fn drop_topic(s: &mut Box<dyn Session>, topic: &str) {
    let gone = format!("{PREFIX}GONE");
    let _ = exec(s, &format!("DROP STREAM IF EXISTS `{gone}`")).await;
    let made = exec(
        s,
        &format!(
            "CREATE STREAM `{gone}` (v STRING) WITH (KAFKA_TOPIC='{}', VALUE_FORMAT='KAFKA')",
            topic.replace('\'', "''")
        ),
    )
    .await;
    if made.is_ok() {
        let _ = exec(s, &format!("DROP STREAM `{gone}` DELETE TOPIC")).await;
    }
}

/// Terminates the test's queries and drops its streams and tables with
/// their topics, until nothing of the test is left.
async fn cleanup(c: &ConnectionConfig) {
    let mut s = open(c).await;
    for _ in 0..5 {
        if let Ok(list) = s.jobs().await {
            for j in &list.jobs {
                if ours(&j.id) || j.group.as_deref().is_some_and(ours) {
                    let _ = s.stop_job(&j.id).await;
                }
            }
        }
        let mut left = 0;
        for (show, kind) in [("TABLES", "TABLE"), ("STREAMS", "STREAM")] {
            for n in names(&mut s, show).await.into_iter().filter(|n| ours(n)) {
                left += 1;
                // A source table can't delete its topic (it reads another
                // object's): it is dropped alone.
                if exec(&mut s, &format!("DROP {kind} IF EXISTS `{n}` DELETE TOPIC"))
                    .await
                    .is_err()
                {
                    let _ = exec(&mut s, &format!("DROP {kind} IF EXISTS `{n}`")).await;
                }
            }
        }
        if left == 0 {
            let topics = topics_left(&mut s).await;
            if topics.is_empty() {
                return;
            }
            for t in &topics {
                drop_topic(&mut s, t).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The form's values, as the app sends them back.
fn values_of(f: &ObjectForm) -> FormValues {
    let mut lists = BTreeMap::new();
    for s in &f.sections {
        for l in &s.lists {
            lists.insert(l.key.clone(), l.rows.clone());
        }
    }
    FormValues {
        values: f.values.clone(),
        lists,
    }
}

fn set(v: &mut FormValues, k: &str, value: Value) {
    v.values.insert(k.into(), value);
}

/// The persistent query `id` as the jobs list has it.
async fn job(s: &mut Box<dyn Session>, id: &str) -> Option<dbine_driver::ServerJob> {
    s.jobs()
        .await
        .expect("jobs")
        .jobs
        .into_iter()
        .find(|j| j.id == id)
}

/// Waits until the query's `enabled` is `want`.
async fn wait_enabled(s: &mut Box<dyn Session>, id: &str, want: bool) {
    let start = Instant::now();
    loop {
        let j = job(s, id).await.expect("listed");
        if j.enabled == want {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "{id} never became enabled={want}: {j:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_gone(s: &mut Box<dyn Session>, id: &str) {
    let start = Instant::now();
    while job(s, id).await.is_some() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "{id} still listed"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn statement_of(s: &mut Box<dyn Session>, id: &str) -> String {
    s.job_script(id).await.expect("script")
}

async fn body(c: ConnectionConfig) {
    let d = &dbine_driver_ksqldb::drivers()[0];
    assert!(d.capabilities().scheduler && d.capabilities().jobs);
    let mut s = open(&c).await;
    exec(
        &mut s,
        &format!("CREATE STREAM {SRC} (id INT KEY, v VARCHAR, n INT) WITH (KAFKA_TOPIC='dbine_test_sched_src', VALUE_FORMAT='JSON', PARTITIONS=1)"),
    )
    .await
    .expect("source");
    exec(
        &mut s,
        &format!("CREATE STREAM {INS} (id INT KEY, v VARCHAR) WITH (KAFKA_TOPIC='dbine_test_sched_ins', VALUE_FORMAT='JSON', PARTITIONS=1)"),
    )
    .await
    .expect("insert target");
    for (i, v) in [(1, "a"), (2, "b"), (3, "a")] {
        exec(
            &mut s,
            &format!("INSERT INTO {SRC} (id, v, n) VALUES ({i}, '{v}', {i})"),
        )
        .await
        .expect("rows");
    }

    // The scheduler: one kind, the jobs' ids, a log.
    let info = s.scheduler_info().await.expect("info");
    assert_eq!(
        info.kinds
            .iter()
            .map(|k| k.key.as_str())
            .collect::<Vec<_>>(),
        ["job"]
    );
    assert!(info.kinds[0].can_create && info.kinds[0].can_edit && info.kinds[0].can_delete);
    assert!(info.logs);
    // No steps, no history; PAUSE / RESUME is the enabled switch.
    let k = &info.kinds[0];
    assert!(
        !k.run_from_step && !k.clear_history && k.actions.is_empty(),
        "{k:?}"
    );
    assert!(s.scheduler_items("schedule").await.is_err());

    // A new CREATE STREAM … AS SELECT.
    let form = s.scheduler_form("job", None).await.expect("new form");
    assert!(!form.read_only);
    assert!(form.choices["stream"].iter().any(|(v, _)| v == INS));
    let mut v = values_of(&form);
    set(&mut v, "statement_kind", json!("create_stream"));
    set(&mut v, "name", json!("dbine_test_sched_out"));
    set(
        &mut v,
        "select",
        json!(format!("SELECT id, v, n FROM {SRC} EMIT CHANGES")),
    );
    set(&mut v, "kafka_topic", json!("dbine_test_sched_out"));
    set(&mut v, "partitions", json!(1));
    v.lists.insert(
        "properties".into(),
        vec![BTreeMap::from([
            ("name".to_string(), json!("processing.guarantee")),
            ("value".to_string(), json!("at_least_once")),
        ])],
    );
    let script = s.scheduler_script("job", None, &v).await.expect("script");
    assert!(script.contains("CREATE STREAM dbine_test_sched_out WITH (KAFKA_TOPIC='dbine_test_sched_out', PARTITIONS=1, VALUE_FORMAT='JSON') AS"), "{script}");
    assert!(script.contains("SET 'processing.guarantee'='at_least_once';"));
    let out_id = s
        .scheduler_save("job", None, &v)
        .await
        .expect("create")
        .expect("its id");
    assert!(out_id.starts_with("CSAS_DBINE_TEST_SCHED_OUT"), "{out_id}");
    let listed = s.scheduler_items("job").await.expect("items");
    assert!(listed
        .items
        .iter()
        .any(|i| i.id == out_id && i.enabled == Some(true)));
    assert!(
        job(&mut s, &out_id).await.is_some(),
        "the jobs list has the same id"
    );

    // Its form reads it back; untouched, nothing to save.
    let form = s.scheduler_form("job", Some(&out_id)).await.expect("form");
    assert_eq!(form.values["statement_kind"], json!("create_stream"));
    assert_eq!(form.values["kafka_topic"], json!("dbine_test_sched_out"));
    assert_eq!(form.values["offset_reset"], json!("earliest"));
    assert_eq!(form.values["state_id"], json!(out_id));
    let props = form
        .sections
        .iter()
        .flat_map(|s| &s.lists)
        .find(|l| l.key == "properties")
        .unwrap();
    assert_eq!(props.rows[0]["name"], json!("processing.guarantee"));
    let v = values_of(&form);
    let script = s.scheduler_script("job", Some(&out_id), &v).await.unwrap();
    assert!(script.contains("no hay cambios"), "{script}");

    // A new SELECT: replaced in place, same id.
    let mut v2 = v.clone();
    set(
        &mut v2,
        "select",
        json!(format!(
            "SELECT id, v, n FROM {SRC} WHERE n > 1 EMIT CHANGES"
        )),
    );
    let script = s.scheduler_script("job", Some(&out_id), &v2).await.unwrap();
    assert!(
        script.contains("CREATE OR REPLACE STREAM `DBINE_TEST_SCHED_OUT`"),
        "{script}"
    );
    let saved = s
        .scheduler_save("job", Some(&out_id), &v2)
        .await
        .expect("replace");
    assert_eq!(saved.as_deref(), Some(out_id.as_str()));
    let st = statement_of(&mut s, &out_id).await;
    assert!(st.contains("WHERE (DBINE_TEST_SCHED_SRC.N > 1)"), "{st}");

    // A form read before that change is refused.
    let e = s
        .scheduler_save("job", Some(&out_id), &v)
        .await
        .expect_err("stale");
    assert!(e.to_string().contains("cambió en el servidor"), "{e}");

    // A change ksqlDB can't make in place says why.
    let form = s.scheduler_form("job", Some(&out_id)).await.unwrap();
    let mut v3 = values_of(&form);
    set(
        &mut v3,
        "select",
        json!(format!(
            "SELECT v, COUNT(*) AS c FROM {SRC} GROUP BY v EMIT CHANGES"
        )),
    );
    assert!(s.scheduler_save("job", Some(&out_id), &v3).await.is_err());
    assert!(
        job(&mut s, &out_id).await.is_some(),
        "the refused change left it"
    );

    // Disable (PAUSE) and enable (RESUME).
    let form = s.scheduler_form("job", Some(&out_id)).await.unwrap();
    let mut v4 = values_of(&form);
    set(&mut v4, "enabled", json!(false));
    let script = s.scheduler_script("job", Some(&out_id), &v4).await.unwrap();
    assert!(
        script.trim_end().ends_with(&format!("PAUSE {out_id};")),
        "{script}"
    );
    s.scheduler_save("job", Some(&out_id), &v4)
        .await
        .expect("pause");
    wait_enabled(&mut s, &out_id, false).await;
    let form = s.scheduler_form("job", Some(&out_id)).await.unwrap();
    assert_eq!(form.values["enabled"], json!(false));
    let mut v5 = values_of(&form);
    set(&mut v5, "enabled", json!(true));
    s.scheduler_save("job", Some(&out_id), &v5)
        .await
        .expect("resume");
    wait_enabled(&mut s, &out_id, true).await;

    // A new destination: recreated, with a new id.
    let form = s.scheduler_form("job", Some(&out_id)).await.unwrap();
    let mut v6 = values_of(&form);
    set(&mut v6, "kafka_topic", json!("dbine_test_sched_out2"));
    let script = s.scheduler_script("job", Some(&out_id), &v6).await.unwrap();
    assert!(script.starts_with("-- Se recrea"), "{script}");
    assert!(script.contains(&format!(
        "TERMINATE {out_id};\nDROP STREAM `DBINE_TEST_SCHED_OUT`;"
    )));
    let new_id = s
        .scheduler_save("job", Some(&out_id), &v6)
        .await
        .expect("recreate")
        .expect("new id");
    assert_ne!(new_id, out_id);
    wait_gone(&mut s, &out_id).await;
    let form = s.scheduler_form("job", Some(&new_id)).await.unwrap();
    assert_eq!(form.values["kafka_topic"], json!("dbine_test_sched_out2"));

    // A recreate whose new statement ksqlDB refuses (a column that doesn't
    // exist), after the TERMINATE and the DROP ran: the old query is made
    // again as it was, with its properties.
    let mut refused = values_of(&form);
    set(&mut refused, "kafka_topic", json!("dbine_test_sched_out3"));
    set(
        &mut refused,
        "select",
        json!(format!("SELECT idd FROM {SRC} EMIT CHANGES")),
    );
    let e = s
        .scheduler_save("job", Some(&new_id), &refused)
        .await
        .expect_err("refused")
        .to_string();
    assert!(e.starts_with("ksqlDB rechazó el cambio: "), "{e}");
    assert!(e.contains("La volvió a crear como estaba"), "{e}");
    wait_gone(&mut s, &new_id).await;
    let new_id = s
        .scheduler_items("job")
        .await
        .expect("items")
        .items
        .into_iter()
        .map(|i| i.id)
        .find(|id| id.starts_with("CSAS_DBINE_TEST_SCHED_OUT") && *id != new_id)
        .expect("made again");
    assert!(e.contains(&new_id), "{e}");
    let form = s.scheduler_form("job", Some(&new_id)).await.unwrap();
    assert_eq!(form.values["kafka_topic"], json!("dbine_test_sched_out2"));
    assert!(
        form.values["select"]
            .as_str()
            .is_some_and(|q| q.contains(".N > 1)")),
        "{:?}",
        form.values["select"]
    );
    let props = form
        .sections
        .iter()
        .flat_map(|s| &s.lists)
        .find(|l| l.key == "properties")
        .unwrap();
    assert_eq!(props.rows[0]["name"], json!("processing.guarantee"));

    // A CREATE TABLE … AS SELECT, deleted (TERMINATE) from the designer.
    let form = s.scheduler_form("job", None).await.unwrap();
    let mut t = values_of(&form);
    set(&mut t, "statement_kind", json!("create_table"));
    set(&mut t, "name", json!("dbine_test_sched_tbl"));
    set(
        &mut t,
        "select",
        json!(format!(
            "SELECT v, COUNT(*) AS c FROM {SRC} GROUP BY v EMIT CHANGES"
        )),
    );
    let tbl_id = s
        .scheduler_save("job", None, &t)
        .await
        .expect("table")
        .expect("id");
    assert!(tbl_id.starts_with("CTAS_DBINE_TEST_SCHED_TBL"), "{tbl_id}");
    let del = s.scheduler_delete_script("job", &tbl_id).await.unwrap();
    assert!(
        del.trim_end().ends_with(&format!("TERMINATE {tbl_id};")),
        "{del}"
    );
    s.scheduler_delete("job", &tbl_id).await.expect("delete");
    wait_gone(&mut s, &tbl_id).await;
    assert!(
        names(&mut s, "TABLES")
            .await
            .iter()
            .any(|n| n == "DBINE_TEST_SCHED_TBL"),
        "the table stays"
    );

    // An INSERT INTO with its own id, created paused; a change keeps the id.
    let form = s.scheduler_form("job", None).await.unwrap();
    let mut i = values_of(&form);
    set(&mut i, "statement_kind", json!("insert"));
    set(&mut i, "target", json!(INS));
    set(&mut i, "query_id", json!("dbine_test_sched_q1"));
    set(
        &mut i,
        "select",
        json!(format!("SELECT id, v FROM {SRC} EMIT CHANGES")),
    );
    set(&mut i, "enabled", json!(false));
    let ins_id = s
        .scheduler_save("job", None, &i)
        .await
        .expect("insert")
        .expect("id");
    assert_eq!(ins_id, "DBINE_TEST_SCHED_Q1");
    wait_enabled(&mut s, &ins_id, false).await;
    // The same id again is refused before anything runs.
    assert!(s.scheduler_script("job", None, &i).await.is_err());
    let form = s.scheduler_form("job", Some(&ins_id)).await.unwrap();
    assert_eq!(form.values["statement_kind"], json!("insert"));
    assert_eq!(form.values["target"], json!(INS));
    let mut i2 = values_of(&form);
    set(
        &mut i2,
        "select",
        json!(format!("SELECT id, UCASE(v) AS v FROM {SRC} EMIT CHANGES")),
    );
    let script = s.scheduler_script("job", Some(&ins_id), &i2).await.unwrap();
    assert!(
        script.starts_with("-- ksqlDB no cambia una INSERT INTO"),
        "{script}"
    );
    let again = s
        .scheduler_save("job", Some(&ins_id), &i2)
        .await
        .expect("insert again");
    assert_eq!(again.as_deref(), Some(ins_id.as_str()));
    wait_enabled(&mut s, &ins_id, false).await;
    assert!(statement_of(&mut s, &ins_id).await.contains("UCASE(v)"));
    // An INSERT INTO ksqlDB refuses after its TERMINATE: made again with
    // its id, paused as it was.
    let form = s.scheduler_form("job", Some(&ins_id)).await.unwrap();
    let mut i3 = values_of(&form);
    set(
        &mut i3,
        "select",
        json!(format!("SELECT id, nope FROM {SRC} EMIT CHANGES")),
    );
    let e = s
        .scheduler_save("job", Some(&ins_id), &i3)
        .await
        .expect_err("refused")
        .to_string();
    assert!(e.contains("La volvió a crear como estaba."), "{e}");
    wait_enabled(&mut s, &ins_id, false).await;
    assert!(statement_of(&mut s, &ins_id).await.contains("UCASE(v)"));

    // The log reads.
    let logs = s.scheduler_logs().await.expect("logs");
    assert_eq!(logs[0].columns.len(), 4);

    // Odd ids and kinds are refused before anything runs.
    for bad in [
        "CSAS_X; DROP STREAM DBINE_TEST_SCHED_SRC",
        "CSAS X",
        "",
        "CSAS_DBINE_TEST_SCHED_NOPE_1",
    ] {
        assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad:?}");
        assert!(
            s.scheduler_delete_script("job", bad).await.is_err(),
            "{bad:?}"
        );
        assert!(s.scheduler_delete("job", bad).await.is_err(), "{bad:?}");
    }
    // A SELECT that smuggles a second statement is refused.
    let mut evil = values_of(&s.scheduler_form("job", Some(&new_id)).await.unwrap());
    set(
        &mut evil,
        "select",
        json!(format!(
            "SELECT * FROM {SRC} EMIT CHANGES; DROP STREAM {SRC}"
        )),
    );
    assert!(s.scheduler_save("job", Some(&new_id), &evil).await.is_err());
    assert!(names(&mut s, "STREAMS").await.iter().any(|n| n == SRC));
    // No steps and no history in ksqlDB.
    assert!(s.run_job_from(&new_id, "x").await.is_err());
    assert!(s.clear_job_history(&new_id).await.is_err());

    // Read-only: the forms show, nothing changes.
    let mut ro = ReadOnlySession::new(open(&c).await);
    let form = ro
        .scheduler_form("job", Some(&new_id))
        .await
        .expect("ro form");
    assert!(form.read_only);
    let v = values_of(&form);
    assert!(ro.scheduler_script("job", Some(&new_id), &v).await.is_ok());
    for r in [
        ro.scheduler_save("job", Some(&new_id), &v)
            .await
            .map(|_| ()),
        ro.scheduler_delete("job", &new_id).await,
    ] {
        assert!(r.is_err(), "{r:?}");
    }

    // The query a CREATE SOURCE TABLE keeps running (over the source
    // stream's topic): ksqlDB ends it only with its table, so the tree
    // lists it read-only and the designer neither opens nor deletes it.
    exec(
        &mut s,
        &format!("CREATE SOURCE TABLE {CST} (id INT PRIMARY KEY, v VARCHAR, n INT) WITH (KAFKA_TOPIC='dbine_test_sched_src', VALUE_FORMAT='JSON')"),
    )
    .await
    .expect("source table");
    let cst = s
        .scheduler_items("job")
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id.starts_with(&format!("CST_{CST}")))
        .expect("its query listed");
    assert_eq!(
        (cst.can_edit, cst.can_delete),
        (Some(false), Some(false)),
        "{cst:?}"
    );
    assert!(s.scheduler_form("job", Some(&cst.id)).await.is_err());
    assert!(s.scheduler_delete("job", &cst.id).await.is_err());
    // Its job row offers neither Detener (TERMINATE) nor the enabled switch
    // (PAUSE / RESUME), and both are refused before reaching the server.
    let row = job(&mut s, &cst.id).await.expect("its job listed");
    assert!(!row.can_stop && !row.can_enable, "{row:?}");
    assert!(s.stop_job(&cst.id).await.is_err());
    assert!(s.set_job_enabled(&cst.id, false).await.is_err());
    assert!(job(&mut s, &cst.id).await.is_some_and(|j| j.enabled));
    // Its own items stay as the kind says.
    let own = s
        .scheduler_items("job")
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id == new_id)
        .expect("listed");
    assert_eq!((own.can_edit, own.can_delete), (None, None));
    exec(&mut s, &format!("DROP TABLE {CST}"))
        .await
        .expect("drop the source table");
    wait_gone(&mut s, &cst.id).await;

    // Delete the rest from the designer.
    for id in [new_id, ins_id] {
        s.scheduler_delete("job", &id).await.expect("delete");
        wait_gone(&mut s, &id).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn persistent_queries_in_the_designer() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_KSQLDB_URL not set; skipping");
        return;
    };
    cleanup(&c).await;
    let r = tokio::spawn(body(c.clone())).await;
    cleanup(&c).await;
    let mut s = open(&c).await;
    let mut left: Vec<String> = Vec::new();
    for what in ["STREAMS", "TABLES"] {
        left.extend(names(&mut s, what).await.into_iter().filter(|n| ours(n)));
    }
    left.extend(topics_left(&mut s).await);
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
    assert!(left.is_empty(), "left behind: {left:?}");
}

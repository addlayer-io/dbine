//! The scheduler designer (reflections, table and source refresh policies)
//! against Dremio OSS. Skipped without `DBINE_TEST_DREMIO_URL` (the user is
//! the one tests/processes.rs bootstraps):
//!
//! ```sh
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 \
//!   cargo test -p dbine-driver-dremio --test scheduler -- --ignored
//! ```
//!
//! The test makes a table in `$scratch`, a view in the `dbine_sp` space, a
//! NAS source and their reflections, all named `dbine_test_sched_*`, and
//! drops them at the end, also when an assertion fails.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, QueryOutcome, Session};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

const TABLE: &str = "\"$scratch\".dbine_test_sched_t";
const TABLE_LABEL: &str = "$scratch.dbine_test_sched_t";
const VIEW: &str = "dbine_sp.dbine_test_sched_v";
const SOURCE: &str = "dbine_test_sched_src";
const USER: &str = "dbine";
const PASSWORD: &str = "secreto123";

fn base() -> Option<String> {
    std::env::var("DBINE_TEST_DREMIO_URL")
        .ok()
        .map(|u| u.trim_end_matches('/').to_string())
}

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&base()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASSWORD.into()),
        ..Default::default()
    })
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    dbine_driver_dremio::drivers()[0]
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

/// Dremio's REST API, as the test's own user (to make and drop the source).
async fn rest(method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
    let base = base().unwrap();
    let http = reqwest::Client::new();
    let login: Value = http
        .post(format!("{base}/apiv2/login"))
        .json(&json!({"userName": USER, "password": PASSWORD}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = format!("_dremio{}", login["token"].as_str().unwrap());
    let mut rb = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", token);
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    let resp = rb.send().await.unwrap();
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

async fn source_id() -> Option<String> {
    let v = rest(
        reqwest::Method::GET,
        &format!("/api/v3/catalog/by-path/{SOURCE}"),
        None,
    )
    .await;
    v.get("id").and_then(Value::as_str).map(str::to_string)
}

async fn cleanup(c: &ConnectionConfig) {
    let mut s = open(c).await;
    let _ = exec(&mut s, &format!("DROP VIEW IF EXISTS {VIEW}")).await;
    // Dropping the table drops its reflections.
    let _ = exec(&mut s, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
    if let Some(id) = source_id().await {
        rest(
            reqwest::Method::DELETE,
            &format!("/api/v3/catalog/{id}"),
            None,
        )
        .await;
    }
}

/// What the test leaves: reflections, the table, the view, the source.
async fn left(c: &ConnectionConfig) -> usize {
    let mut s = open(c).await;
    let out = exec(
        &mut s,
        "SELECT COUNT(*) AS n FROM sys.reflections WHERE reflection_name LIKE 'dbine_test_sched%' OR dataset_name LIKE '%dbine_test_sched%'",
    )
    .await
    .expect("count");
    let n = out.results.last().unwrap().rows[0][0]
        .as_i64()
        .or_else(|| {
            out.results.last().unwrap().rows[0][0]
                .as_str()?
                .parse()
                .ok()
        })
        .unwrap_or(0) as usize;
    let tables = exec(
        &mut s,
        "SELECT COUNT(*) AS n FROM INFORMATION_SCHEMA.\"TABLES\" WHERE TABLE_NAME LIKE 'dbine_test_sched%'",
    )
    .await
    .expect("tables");
    let t = tables.results.last().unwrap().rows[0][0]
        .as_i64()
        .or_else(|| {
            tables.results.last().unwrap().rows[0][0]
                .as_str()?
                .parse()
                .ok()
        })
        .unwrap_or(0) as usize;
    n + t + usize::from(source_id().await.is_some())
}

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

fn row(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// The pick for a dataset, as the form's choices list it.
fn pick(f: &ObjectForm, source: &str, label: &str) -> String {
    f.choices[source]
        .iter()
        .find(|(_, l)| l == label)
        .unwrap_or_else(|| panic!("{label} isn't offered: {:?}", f.choices[source]))
        .0
        .clone()
}

async fn ids(s: &mut Box<dyn Session>, kind: &str) -> Vec<String> {
    s.scheduler_items(kind)
        .await
        .expect("items")
        .items
        .into_iter()
        .map(|i| i.id)
        .collect()
}

/// How many refreshes of the reflection Dremio keeps (its job's runs).
async fn refreshes(s: &mut Box<dyn Session>, id: &str) -> usize {
    s.job_runs(id, 50).await.map(|r| r.len()).unwrap_or(0)
}

async fn body(c: ConnectionConfig) {
    let d = &dbine_driver_dremio::drivers()[0];
    assert!(d.capabilities().scheduler && d.capabilities().jobs);
    let mut s = open(&c).await;
    exec(
        &mut s,
        &format!("CREATE TABLE {TABLE} AS SELECT * FROM (VALUES (1, CAST(2.5 AS DOUBLE), TIMESTAMP '2026-01-01 10:00:00', 'x'), (2, CAST(3.5 AS DOUBLE), TIMESTAMP '2026-01-02 11:00:00', 'y')) AS t(a, b, ts, c)"),
    )
    .await
    .expect("table");
    exec(
        &mut s,
        &format!("CREATE VIEW {VIEW} AS SELECT a, c FROM {TABLE}"),
    )
    .await
    .expect("view");
    rest(
        reqwest::Method::POST,
        "/api/v3/catalog",
        Some(json!({"entityType": "source", "name": SOURCE, "type": "NAS", "config": {"path": "/tmp"}})),
    )
    .await;
    assert!(source_id().await.is_some(), "the source");

    // The scheduler's kinds and its log.
    let info = s.scheduler_info().await.expect("info");
    assert_eq!(
        info.kinds
            .iter()
            .map(|k| k.key.as_str())
            .collect::<Vec<_>>(),
        ["job", "table_policy", "source_policy"]
    );
    assert!(!info.kinds[2].can_create && info.kinds[2].can_edit && !info.kinds[2].can_delete);
    assert!(info.logs);
    // A reflection's "run now" is the job's; a table policy refreshes its
    // table's reflections now. No steps; Dremio expires the history itself.
    let actions: Vec<Vec<&str>> = info
        .kinds
        .iter()
        .map(|k| k.actions.iter().map(|a| a.key.as_str()).collect())
        .collect();
    assert_eq!(actions, [vec![], vec!["refresh"], vec![]]);
    assert!(info
        .kinds
        .iter()
        .all(|k| !k.run_from_step && !k.clear_history));

    // A new RAW reflection: typed columns, partition transforms, striped.
    let form = s.scheduler_form("job", None).await.expect("new form");
    let mut v = values_of(&form);
    set(&mut v, "name", json!("dbine_test_sched_raw"));
    set(
        &mut v,
        "dataset",
        json!(pick(&form, "dataset", TABLE_LABEL)),
    );
    set(&mut v, "reflection_type", json!("RAW"));
    set(&mut v, "display", json!("a, C, ts"));
    set(&mut v, "strategy", json!("STRIPED"));
    set(&mut v, "sort", json!("a"));
    set(&mut v, "distribution", json!("c"));
    v.lists.insert(
        "partitions".into(),
        vec![
            row(&[("column", json!("ts")), ("transform", json!("DAY"))]),
            row(&[
                ("column", json!("a")),
                ("transform", json!("BUCKET")),
                ("argument", json!(4)),
            ]),
        ],
    );
    let script = s.scheduler_script("job", None, &v).await.expect("script");
    assert!(
        script.contains("ALTER TABLE \"$scratch\".\"dbine_test_sched_t\" CREATE RAW REFLECTION \"dbine_test_sched_raw\" USING DISPLAY (\"a\", \"c\", \"ts\") DISTRIBUTE BY (\"c\") STRIPED PARTITION BY (DAY(\"ts\"), BUCKET(4, \"a\")) LOCALSORT BY (\"a\");"),
        "{script}"
    );
    let raw_id = s
        .scheduler_save("job", None, &v)
        .await
        .expect("create")
        .expect("its id");
    assert!(raw_id.starts_with("reflection:"), "{raw_id}");
    assert!(ids(&mut s, "job").await.contains(&raw_id));
    let jobs = s.jobs().await.expect("jobs");
    assert!(
        jobs.jobs.iter().any(|j| j.id == raw_id),
        "same id as the jobs"
    );

    // The job's script recreates it exactly (transforms, strategy).
    let js = s.job_script(&raw_id).await.expect("job script");
    assert!(
        js.contains("STRIPED PARTITION BY (DAY(\"ts\"), BUCKET(4, \"a\"))"),
        "{js}"
    );

    // Its form reads it back; untouched, nothing to save.
    let form = s.scheduler_form("job", Some(&raw_id)).await.expect("form");
    assert_eq!(form.values["display"], json!(["a", "c", "ts"]));
    assert_eq!(form.values["strategy"], json!("STRIPED"));
    assert_eq!(form.values["dataset_label"], json!(TABLE_LABEL));
    assert!(form.choices["column"].iter().any(|(c, _)| c == "b"));
    let v = values_of(&form);
    let script = s.scheduler_script("job", Some(&raw_id), &v).await.unwrap();
    assert!(script.contains("no hay cambios"), "{script}");

    // A change: renamed, a column less, disabled — in place, same id.
    let mut v2 = v.clone();
    set(&mut v2, "name", json!("dbine_test_sched_raw2"));
    set(&mut v2, "display", json!(["a", "c"]));
    set(&mut v2, "enabled", json!(false));
    v2.lists.insert(
        "partitions".into(),
        vec![row(&[
            ("column", json!("c")),
            ("transform", json!("TRUNCATE")),
            ("argument", json!(1)),
        ])],
    );
    let script = s.scheduler_script("job", Some(&raw_id), &v2).await.unwrap();
    assert!(
        script.contains(&format!(
            "-- PUT /api/v3/reflection/{}",
            raw_id.trim_start_matches("reflection:")
        )),
        "{script}"
    );
    let saved = s
        .scheduler_save("job", Some(&raw_id), &v2)
        .await
        .expect("edit");
    assert_eq!(saved.as_deref(), Some(raw_id.as_str()));
    let form = s.scheduler_form("job", Some(&raw_id)).await.unwrap();
    assert_eq!(form.values["name"], json!("dbine_test_sched_raw2"));
    assert_eq!(form.values["display"], json!(["a", "c"]));
    assert_eq!(form.values["enabled"], json!(false));
    // A form read before that change is refused.
    let e = s
        .scheduler_save("job", Some(&raw_id), &v)
        .await
        .expect_err("stale");
    assert!(e.to_string().contains("cambió en el servidor"), "{e}");
    // Enabled again.
    let mut v3 = values_of(&form);
    set(&mut v3, "enabled", json!(true));
    s.scheduler_save("job", Some(&raw_id), &v3)
        .await
        .expect("enable");

    // An aggregation reflection with granularity and measure functions.
    let form = s.scheduler_form("job", None).await.unwrap();
    let mut a = values_of(&form);
    set(&mut a, "name", json!("dbine_test_sched_agg"));
    set(
        &mut a,
        "dataset",
        json!(pick(&form, "dataset", TABLE_LABEL)),
    );
    set(&mut a, "reflection_type", json!("AGGREGATION"));
    a.lists.insert(
        "dimensions".into(),
        vec![
            row(&[("column", json!("ts")), ("granularity", json!("DATE"))]),
            row(&[("column", json!("c")), ("granularity", json!("NORMAL"))]),
        ],
    );
    a.lists.insert(
        "measures".into(),
        vec![
            row(&[
                ("column", json!("b")),
                ("functions", json!(["SUM", "MIN", "APPROX_COUNT_DISTINCT"])),
            ]),
            row(&[("column", json!("a")), ("functions", json!([]))]),
        ],
    );
    let script = s.scheduler_script("job", None, &a).await.unwrap();
    assert!(script.contains("USING DIMENSIONS (\"ts\" BY DAY, \"c\") MEASURES (\"b\" (SUM, MIN, APPROXIMATE COUNT DISTINCT), \"a\")"), "{script}");
    let agg_id = s
        .scheduler_save("job", None, &a)
        .await
        .expect("agg")
        .expect("id");
    let form = s.scheduler_form("job", Some(&agg_id)).await.unwrap();
    let dims = form
        .sections
        .iter()
        .flat_map(|x| &x.lists)
        .find(|l| l.key == "dimensions")
        .unwrap();
    assert_eq!(dims.rows[0]["granularity"], json!("DATE"));
    let ms = form
        .sections
        .iter()
        .flat_map(|x| &x.lists)
        .find(|l| l.key == "measures")
        .unwrap();
    assert_eq!(
        ms.rows[0]["functions"],
        json!(["SUM", "MIN", "APPROX_COUNT_DISTINCT"])
    );
    // Its type can't change.
    let mut t = values_of(&form);
    set(&mut t, "reflection_type", json!("RAW"));
    let script = s.scheduler_script("job", Some(&agg_id), &t).await.unwrap();
    assert!(
        script.contains("no hay cambios"),
        "the type is the reflection's: {script}"
    );

    // Created disabled: SQL can't, the REST API does.
    let form = s.scheduler_form("job", None).await.unwrap();
    let mut off = values_of(&form);
    set(&mut off, "name", json!("dbine_test_sched_off"));
    set(
        &mut off,
        "dataset",
        json!(pick(&form, "dataset", TABLE_LABEL)),
    );
    set(&mut off, "display", json!("a"));
    set(&mut off, "enabled", json!(false));
    let script = s.scheduler_script("job", None, &off).await.unwrap();
    assert!(script.contains("-- POST /api/v3/reflection"), "{script}");
    let off_id = s
        .scheduler_save("job", None, &off)
        .await
        .expect("off")
        .expect("id");
    let form = s.scheduler_form("job", Some(&off_id)).await.unwrap();
    assert_eq!(form.values["enabled"], json!(false));

    // A reflection on a view, deleted from the designer (by id).
    let form = s.scheduler_form("job", None).await.unwrap();
    let mut vr = values_of(&form);
    set(&mut vr, "name", json!("dbine_test_sched_vr"));
    set(
        &mut vr,
        "dataset",
        json!(pick(&form, "dataset", "dbine_sp.dbine_test_sched_v")),
    );
    set(&mut vr, "display", json!("a, c"));
    let vr_id = s
        .scheduler_save("job", None, &vr)
        .await
        .expect("view refl")
        .expect("id");
    let del = s.scheduler_delete_script("job", &vr_id).await.unwrap();
    assert!(
        del.contains(&format!(
            "DROP REFLECTION \"{}\"",
            vr_id.trim_start_matches("reflection:")
        )),
        "{del}"
    );
    s.scheduler_delete("job", &vr_id).await.expect("delete");
    assert!(!ids(&mut s, "job").await.contains(&vr_id));

    // A table's own refresh policy: made, read, changed, deleted.
    let form = s
        .scheduler_form("table_policy", None)
        .await
        .expect("new policy");
    let mut p = values_of(&form);
    set(&mut p, "table", json!(pick(&form, "table", TABLE_LABEL)));
    set(&mut p, "policy_type", json!("SCHEDULE"));
    set(&mut p, "refresh_schedule", json!("0 15 3 * * ?"));
    set(&mut p, "grace_period", json!(86_400));
    let script = s.scheduler_script("table_policy", None, &p).await.unwrap();
    assert!(script.contains("-- PUT /api/v3/catalog/"), "{script}");
    assert!(
        script.contains("\"refreshSchedule\": \"0 15 3 * * ?\""),
        "{script}"
    );
    let pol_id = s
        .scheduler_save("table_policy", None, &p)
        .await
        .expect("policy")
        .expect("id");
    assert!(pol_id.starts_with("table:"), "{pol_id}");
    assert!(ids(&mut s, "table_policy").await.contains(&pol_id));
    assert!(
        s.scheduler_save("table_policy", None, &p).await.is_err(),
        "it has one already"
    );
    // Its table's reflections refreshed now: the REST request, shown, then
    // sent.
    let sc = s
        .scheduler_action_script("table_policy", &pol_id, "refresh", &Default::default())
        .await
        .expect("action script");
    let uuid = pol_id.trim_start_matches("table:");
    assert!(
        sc.contains(&format!("-- POST /api/v3/catalog/{uuid}/refresh")),
        "{sc}"
    );
    assert!(sc.lines().all(|l| l.starts_with("-- ")), "{sc}");
    let before = refreshes(&mut s, &raw_id).await;
    s.scheduler_action("table_policy", &pol_id, "refresh", &Default::default())
        .await
        .expect("refresh now");
    let start = std::time::Instant::now();
    while refreshes(&mut s, &raw_id).await <= before {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "no new refresh of {raw_id} after the action"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    for (kind, id, action) in [
        ("table_policy", pol_id.as_str(), "drop"),
        ("table_policy", "table:../../catalog", "refresh"),
        ("job", raw_id.as_str(), "refresh"),
        ("source_policy", "source:x", "refresh"),
    ] {
        assert!(
            s.scheduler_action(kind, id, action, &Default::default())
                .await
                .is_err(),
            "{kind} {id} {action}"
        );
    }
    let form = s
        .scheduler_form("table_policy", Some(&pol_id))
        .await
        .unwrap();
    assert_eq!(form.values["policy_type"], json!("SCHEDULE"));
    assert_eq!(form.values["refresh_schedule"], json!("0 15 3 * * ?"));
    assert_eq!(form.values["grace_period"], json!(86_400));
    let mut p2 = values_of(&form);
    set(&mut p2, "policy_type", json!("PERIOD"));
    set(&mut p2, "refresh_period", json!(7200));
    set(&mut p2, "method", json!("INCREMENTAL"));
    s.scheduler_save("table_policy", Some(&pol_id), &p2)
        .await
        .expect("change");
    let form = s
        .scheduler_form("table_policy", Some(&pol_id))
        .await
        .unwrap();
    assert_eq!(form.values["refresh_period"], json!(7200));
    assert_eq!(form.values["method"], json!("INCREMENTAL"));
    // The reflection's form shows the table's policy.
    let rf = s.scheduler_form("job", Some(&raw_id)).await.unwrap();
    assert_eq!(rf.values["state_policy"], json!("EVERY 2 HOURS"));
    // Checked before it's sent.
    let mut bad = values_of(&form);
    set(&mut bad, "refresh_schedule", json!("whenever"));
    set(&mut bad, "policy_type", json!("SCHEDULE"));
    assert!(s
        .scheduler_save("table_policy", Some(&pol_id), &bad)
        .await
        .is_err());
    let del = s
        .scheduler_delete_script("table_policy", &pol_id)
        .await
        .unwrap();
    assert!(del.contains("sigue la de su origen"), "{del}");
    s.scheduler_delete("table_policy", &pol_id)
        .await
        .expect("delete policy");
    assert!(!ids(&mut s, "table_policy").await.contains(&pol_id));
    assert!(
        s.scheduler_delete("table_policy", &pol_id).await.is_err(),
        "none left"
    );

    // A source's policy.
    let src = format!("source:{}", source_id().await.unwrap());
    assert!(ids(&mut s, "source_policy").await.contains(&src));
    let form = s.scheduler_form("source_policy", Some(&src)).await.unwrap();
    assert_eq!(form.values["name"], json!(SOURCE));
    let untouched = values_of(&form);
    assert!(s
        .scheduler_script("source_policy", Some(&src), &untouched)
        .await
        .unwrap()
        .contains("no hay cambios"));
    let mut sp = untouched.clone();
    set(&mut sp, "policy_type", json!("NEVER"));
    set(&mut sp, "never_expire", json!(true));
    let script = s
        .scheduler_script("source_policy", Some(&src), &sp)
        .await
        .unwrap();
    assert!(
        script.contains("\"accelerationNeverRefresh\": true"),
        "{script}"
    );
    s.scheduler_save("source_policy", Some(&src), &sp)
        .await
        .expect("source policy");
    let form = s.scheduler_form("source_policy", Some(&src)).await.unwrap();
    assert_eq!(form.values["policy_type"], json!("NEVER"));
    assert_eq!(form.values["never_expire"], json!(true));
    let mut sp2 = values_of(&form);
    set(&mut sp2, "policy_type", json!("PERIOD"));
    set(&mut sp2, "refresh_period", json!(7200));
    set(&mut sp2, "never_expire", json!(false));
    set(&mut sp2, "grace_period", json!(21_600));
    s.scheduler_save("source_policy", Some(&src), &sp2)
        .await
        .expect("back");
    let form = s.scheduler_form("source_policy", Some(&src)).await.unwrap();
    assert_eq!(form.values["refresh_period"], json!(7200));
    assert_eq!(form.values["grace_period"], json!(21_600));
    assert!(
        s.scheduler_form("source_policy", None).await.is_err(),
        "no new sources here"
    );
    assert!(s.scheduler_delete("source_policy", &src).await.is_err());

    // The log: the reflections' refreshes.
    let logs = s.scheduler_logs().await.expect("logs");
    assert_eq!(logs[0].columns.len(), 6);
    assert!(s.clear_job_history(&raw_id).await.is_err());

    // Odd ids and kinds are refused before anything runs.
    for (kind, bad) in [
        ("job", "reflection:x' OR '1'='1"),
        ("job", "reflection:00000000-0000-0000-0000-000000000000"),
        ("job", "dbine_test_sched_raw2"),
        ("table_policy", "table:../../catalog"),
        ("table_policy", "table:00000000-0000-0000-0000-000000000000"),
        ("source_policy", "source:x"),
        ("schedule", "x"),
    ] {
        assert!(
            s.scheduler_form(kind, Some(bad)).await.is_err(),
            "{kind} {bad}"
        );
        assert!(
            s.scheduler_delete_script(kind, bad).await.is_err(),
            "{kind} {bad}"
        );
        assert!(s.scheduler_delete(kind, bad).await.is_err(), "{kind} {bad}");
    }
    // A name can't break out of its quotes.
    let form = s.scheduler_form("job", None).await.unwrap();
    let mut evil = values_of(&form);
    set(
        &mut evil,
        "name",
        json!("x\" USING DISPLAY (a); DROP TABLE t; --"),
    );
    set(
        &mut evil,
        "dataset",
        json!(pick(&form, "dataset", TABLE_LABEL)),
    );
    set(&mut evil, "display", json!("a; DROP TABLE x"));
    assert!(
        s.scheduler_save("job", None, &evil).await.is_err(),
        "no such column"
    );
    // A pick can't walk out of the catalog's by-path call.
    set(&mut evil, "dataset", json!("\"..\".\"..\".\"reflection\""));
    set(&mut evil, "display", json!("a"));
    let e = s
        .scheduler_script("job", None, &evil)
        .await
        .expect_err("a path out of the catalog");
    assert!(e.to_string().contains("no es una tabla o vista"), "{e}");

    // Read-only: the forms show, nothing changes.
    let mut ro = ReadOnlySession::new(open(&c).await);
    let form = ro
        .scheduler_form("job", Some(&raw_id))
        .await
        .expect("ro form");
    assert!(form.read_only);
    let v = values_of(&form);
    assert!(ro.scheduler_script("job", Some(&raw_id), &v).await.is_ok());
    for r in [
        ro.scheduler_save("job", Some(&raw_id), &v)
            .await
            .map(|_| ()),
        ro.scheduler_delete("job", &raw_id).await,
        ro.scheduler_action("table_policy", &pol_id, "refresh", &Default::default())
            .await,
    ] {
        assert!(r.is_err(), "{r:?}");
    }
    assert!(ro
        .scheduler_action_script("table_policy", &pol_id, "refresh", &Default::default())
        .await
        .is_ok());

    // The rest deleted from the designer.
    for id in [raw_id, agg_id, off_id] {
        s.scheduler_delete("job", &id).await.expect("delete");
        assert!(!ids(&mut s, "job").await.contains(&id));
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn reflections_and_policies_in_the_designer() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_DREMIO_URL not set; skipping");
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

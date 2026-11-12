//! OrientDB's scheduler (OSchedule events) managed through the scheduler
//! contract, against a real server (see tests/integration.rs, which
//! creates the `dbine_admin` database):
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test scheduler -- --ignored
//! ```
//!
//! Every object the test makes is named `dbine_test_sched_*` (the log
//! class `DbineTestSchedLog`) and is deleted at the end, whether the test
//! passed or not.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, FormValues, QueryOutcome, Session};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const FN1: &str = "dbine_test_sched_fn1";
const FN2: &str = "dbine_test_sched_fn2";
const EVENT: &str = "dbine_test_sched_event";
const RENAMED: &str = "dbine_test_sched_renamed";
const CLASS: &str = "DbineTestSchedLog";

fn cfg(url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    }
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_orientdb::drivers().remove(0);
    match d.connect(c, Some("dbine_admin")).await {
        Ok(s) => s,
        Err(_) => {
            let _ = d
                .connect(c, None)
                .await
                .expect("connect")
                .create_database("dbine_admin")
                .await;
            d.connect(c, Some("dbine_admin")).await.expect("connect")
        }
    }
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await?;
    match out.error.take() {
        Some(e) => Err(dbine_driver::Error::Query(e)),
        None => Ok(out),
    }
}

/// The last result's rows.
async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    let out = exec(s, sql).await.expect(sql);
    out.results
        .iter()
        .rfind(|r| !r.columns.is_empty())
        .map(|r| r.rows.clone())
        .unwrap_or_default()
}

async fn cleanup(c: &ConnectionConfig) -> usize {
    let mut s = open(c).await;
    let _ = exec(
        &mut s,
        "DELETE FROM OSchedule WHERE name LIKE 'dbine_test_sched_%'",
    )
    .await;
    let _ = exec(
        &mut s,
        "DELETE FROM OFunction WHERE name LIKE 'dbine_test_sched_%'",
    )
    .await;
    let _ = exec(&mut s, &format!("DROP CLASS {CLASS} IF EXISTS UNSAFE")).await;
    let left = rows(
        &mut s,
        "SELECT count(*) AS n FROM OSchedule WHERE name LIKE 'dbine_test_sched_%'",
    )
    .await;
    let funcs = rows(
        &mut s,
        "SELECT count(*) AS n FROM OFunction WHERE name LIKE 'dbine_test_sched_%'",
    )
    .await;
    let n = |r: &Vec<Vec<Value>>| r.first().and_then(|r| r[0].as_u64()).unwrap_or(0) as usize;
    n(&left) + n(&funcs)
}

fn values(v: Value, args: Value) -> FormValues {
    FormValues {
        values: serde_json::from_value(v).unwrap(),
        lists: BTreeMap::from([(
            "arguments".to_string(),
            serde_json::from_value(args).unwrap(),
        )]),
    }
}

/// How many runs logged `tag`.
async fn logged(s: &mut Box<dyn Session>, tag: &str) -> u64 {
    rows(
        s,
        &format!("SELECT count(*) AS n FROM {CLASS} WHERE tag = '{tag}'"),
    )
    .await
    .first()
    .and_then(|r| r[0].as_u64())
    .unwrap_or(0)
}

async fn wait_logged(s: &mut Box<dyn Session>, tag: &str) {
    let start = Instant::now();
    while logged(s, tag).await == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "no run logged {tag}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// `SELECT name, rule, function.name, arguments` of an event.
async fn on_server(s: &mut Box<dyn Session>, name: &str) -> Option<(String, String, Value)> {
    let r = rows(
        s,
        &format!(
            "SELECT rule, function.name AS fn, arguments FROM OSchedule WHERE name = '{name}'"
        ),
    )
    .await;
    let r = r.first()?;
    Some((
        r[0].as_str()?.to_string(),
        r[1].as_str()?.to_string(),
        // The editor shows a map as JSON text.
        match r.get(2) {
            Some(Value::String(t)) => serde_json::from_str(t).unwrap_or(Value::Null),
            Some(v) => v.clone(),
            None => Value::Null,
        },
    ))
}

async fn body(c: ConnectionConfig) {
    let d = dbine_driver_orientdb::drivers().remove(0);
    assert!(d.capabilities().scheduler);
    let mut s = open(&c).await;
    exec(&mut s, &format!("CREATE CLASS {CLASS} IF NOT EXISTS"))
        .await
        .expect("class");
    exec(
        &mut s,
        &format!("CREATE FUNCTION {FN1} \"INSERT INTO {CLASS} SET at = sysdate(), tag = :tag, n = :n\" PARAMETERS [tag, n] LANGUAGE SQL"),
    )
    .await
    .expect("fn1");
    exec(
        &mut s,
        &format!("CREATE FUNCTION {FN2} \"INSERT INTO {CLASS} SET at = sysdate(), tag = 'fn2'\" LANGUAGE SQL"),
    )
    .await
    .expect("fn2");

    // The scheduler: one kind, everything allowed for root.
    let info = s.scheduler_info().await.expect("info");
    assert_eq!(info.kinds.len(), 1);
    let k = &info.kinds[0];
    assert_eq!(
        (k.key.as_str(), k.label.as_str()),
        ("job", "Eventos programados")
    );
    assert!(k.can_create && k.can_edit && k.can_delete);
    assert!(!info.logs);

    // A new event's form: defaults, the functions to pick from.
    let f = s.scheduler_form("job", None).await.expect("new form");
    assert!(!f.read_only);
    assert_eq!(f.values["database"], json!("dbine_admin"));
    assert!(f.choices["function"]
        .iter()
        .any(|(v, l)| v == FN1 && l.contains("tag, n")));

    // Create: the script, then the save; it runs with its arguments.
    let new = values(
        json!({"name": EVENT, "function": FN1, "rule": "0/1 * * * * ?"}),
        json!([
            {"name": "tag", "type": "text", "value": "it's one"},
            {"name": "n", "type": "number", "value": "3"}
        ]),
    );
    let script = s.scheduler_script("job", None, &new).await.expect("script");
    assert!(script.starts_with("/* "), "{script}");
    assert!(script.contains("INSERT INTO OSchedule SET name = 'dbine_test_sched_event'"));
    let id = s
        .scheduler_save("job", None, &new)
        .await
        .expect("create")
        .expect("id");
    assert_eq!(id, EVENT);
    assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));
    let items = s.scheduler_items("job").await.expect("items");
    assert!(items.items.iter().any(|i| i.id == EVENT));
    wait_logged(&mut s, "it\\'s one").await;
    let n = rows(
        &mut s,
        &format!("SELECT n FROM {CLASS} WHERE tag = 'it\\'s one' LIMIT 1"),
    )
    .await;
    assert_eq!(n[0][0], json!(3));
    // The same name again is refused before the server.
    let e = s.scheduler_save("job", None, &new).await.unwrap_err();
    assert!(e.to_string().contains("Ya hay"), "{e}");

    // The existing event's form round-trips: nothing to save.
    let f = s.scheduler_form("job", Some(&id)).await.expect("form");
    assert_eq!(f.values["rule"], json!("0/1 * * * * ?"));
    assert_eq!(f.values["function"], json!(FN1));
    let args = f.sections[1].lists[0].rows.clone();
    assert_eq!(args.len(), 2);
    let same = FormValues {
        values: f.values.clone(),
        lists: BTreeMap::from([("arguments".to_string(), args.clone())]),
    };
    let script = s
        .scheduler_script("job", Some(&id), &same)
        .await
        .expect("same");
    assert!(script.contains("no hay cambios"), "{script}");
    assert_eq!(
        s.scheduler_save("job", Some(&id), &same).await.unwrap(),
        Some(id.clone())
    );

    // Edit the rule and an argument in place.
    let edited = values(
        json!({"name": EVENT, "function": FN1, "rule": "0/2 * * * * ?"}),
        json!([
            {"name": "tag", "type": "text", "value": "two"},
            {"name": "n", "type": "number", "value": "4"}
        ]),
    );
    let script = s.scheduler_script("job", Some(&id), &edited).await.unwrap();
    assert!(script.contains("UPDATE OSchedule SET rule = '0/2 * * * * ?', arguments = {'n': 4, 'tag': 'two'} WHERE name = 'dbine_test_sched_event'"), "{script}");
    s.scheduler_save("job", Some(&id), &edited)
        .await
        .expect("edit");
    let (rule, func, args) = on_server(&mut s, EVENT).await.expect("on server");
    assert_eq!(rule, "0/2 * * * * ?");
    assert_eq!(func, FN1);
    assert_eq!(args, json!({"tag": "two", "n": 4}));
    wait_logged(&mut s, "two").await;

    // Edit the function (no arguments now).
    let fn2 = values(
        json!({"name": EVENT, "function": FN2, "rule": "0/1 * * * * ?"}),
        json!([]),
    );
    s.scheduler_save("job", Some(&id), &fn2)
        .await
        .expect("edit function");
    let (rule, func, args) = on_server(&mut s, EVENT).await.unwrap();
    assert_eq!((rule.as_str(), func.as_str()), ("0/1 * * * * ?", FN2));
    assert_eq!(args, json!({}));
    wait_logged(&mut s, "fn2").await;

    // Rename: created under the new name, the old record gone.
    let renamed = values(
        json!({"name": RENAMED, "function": FN2, "rule": "0 0 3 * * ?"}),
        json!([]),
    );
    let script = s
        .scheduler_script("job", Some(&id), &renamed)
        .await
        .unwrap();
    assert!(script.contains("no permite cambiar el nombre"), "{script}");
    let new_id = s
        .scheduler_save("job", Some(&id), &renamed)
        .await
        .expect("rename")
        .unwrap();
    assert_eq!(new_id, RENAMED);
    assert!(on_server(&mut s, EVENT).await.is_none());
    assert_eq!(on_server(&mut s, RENAMED).await.unwrap().0, "0 0 3 * * ?");
    let jobs = s.jobs().await.unwrap().jobs;
    assert!(jobs.iter().any(|j| j.id == RENAMED) && !jobs.iter().any(|j| j.id == EVENT));
    // The old name can be used again (the scheduler forgot it).
    let again = values(
        json!({"name": EVENT, "function": FN2, "rule": "0 0 4 * * ?"}),
        json!([]),
    );
    assert_eq!(
        s.scheduler_save("job", None, &again)
            .await
            .expect("old name again"),
        Some(EVENT.to_string())
    );

    // Refused before anything runs: bad rule, unknown function, a
    // parameter the function lacks, unknown and malicious ids, kinds.
    for (v, a, why) in [
        (
            json!({"name": "dbine_test_sched_bad", "function": FN1, "rule": "0 0 3 * * *"}),
            json!([]),
            "?",
        ),
        (
            json!({"name": "dbine_test_sched_bad", "function": "nope", "rule": "0 0 3 * * ?"}),
            json!([]),
            "No existe la función",
        ),
        (
            json!({"name": "dbine_test_sched_bad", "function": FN1, "rule": "0 0 3 * * ?"}),
            json!([{"name": "zzz", "value": "1"}]),
            "no tiene el parámetro",
        ),
    ] {
        let e = s
            .scheduler_save("job", None, &values(v, a))
            .await
            .unwrap_err();
        assert!(e.to_string().contains(why), "{e}");
    }
    assert!(on_server(&mut s, "dbine_test_sched_bad").await.is_none());
    for bad in [
        "dbine_test_sched_event' OR '1'='1",
        "x'; DELETE FROM OSchedule; --",
        "dbine_test_sched_nope",
        "",
    ] {
        assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad:?}");
        assert!(
            s.scheduler_delete_script("job", bad).await.is_err(),
            "{bad:?}"
        );
        assert!(s.scheduler_delete("job", bad).await.is_err(), "{bad:?}");
    }
    assert!(s.scheduler_items("schedule").await.is_err());
    assert!(s.run_job_from(EVENT, "x").await.is_err());
    assert!(s.clear_job_history(EVENT).await.is_err());
    assert!(s.scheduler_logs().await.is_err());
    assert_eq!(
        rows(
            &mut s,
            "SELECT count(*) AS n FROM OSchedule WHERE name LIKE 'dbine_test_sched_%'"
        )
        .await[0][0],
        json!(2)
    );

    // A read-only connection reads the form and the scripts, changes
    // nothing.
    let mut ro = ReadOnlySession::new(open(&c).await);
    assert!(ro.scheduler_form("job", Some(EVENT)).await.is_ok());
    assert!(ro.scheduler_delete_script("job", EVENT).await.is_ok());
    assert!(ro.scheduler_save("job", None, &new).await.is_err());
    assert!(ro.scheduler_delete("job", EVENT).await.is_err());
    assert!(on_server(&mut s, EVENT).await.is_some());

    // Delete: the script, then the delete; it stops running.
    for name in [EVENT, RENAMED] {
        let script = s.scheduler_delete_script("job", name).await.unwrap();
        assert!(script.contains(&format!("DELETE FROM OSchedule WHERE name = '{name}'")));
        s.scheduler_delete("job", name).await.expect("delete");
        assert!(on_server(&mut s, name).await.is_none());
    }
    let before = logged(&mut s, "fn2").await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(logged(&mut s, "fn2").await, before, "it still runs");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn the_scheduler_designer_manages_oschedule_events() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else {
        eprintln!("DBINE_TEST_ORIENTDB_URL not set; skipping");
        return;
    };
    let c = cfg(&url);
    cleanup(&c).await;
    let r = tokio::spawn(body(c.clone())).await;
    let left = cleanup(&c).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
    assert_eq!(left, 0, "objects left behind");
}

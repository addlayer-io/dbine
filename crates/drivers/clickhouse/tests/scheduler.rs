//! The scheduler managed through the scheduler contract against real
//! servers: refreshable materialized views on ClickHouse and tasks on
//! Timeplus Proton. Skipped without the URLs:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_CLICKHOUSE_22_URL=http://dbine:dbine@localhost:25124 \
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119 \
//!   cargo test -p dbine-driver-clickhouse --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! Every object a test makes is named `dbine_test_sched_*` and is dropped
//! at the end, whether the test passed or not.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, QueryOutcome, Session};
use futures::FutureExt;
use serde_json::{json, Value};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

fn cfg(env: &str, id: &str) -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let url = reqwest::Url::parse(&url).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn connect(c: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == c.driver)
        .unwrap();
    assert!(d.capabilities().scheduler);
    d.connect(c, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    let r = s.execute(sql, 1000, &mut out).await;
    assert!(
        r.is_ok() && out.errors.is_empty(),
        "{sql}: {r:?} {:?}",
        out.errors
    );
    out
}

/// The last result's rows.
async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    run(s, sql)
        .await
        .results
        .iter()
        .rfind(|r| !r.columns.is_empty())
        .map(|r| r.rows.clone())
        .unwrap_or_default()
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    match rows(s, sql).await.first().and_then(|r| r.first()).cloned() {
        Some(Value::String(t)) => t,
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

/// Runs the test, then the cleanup on a connection of its own, and only
/// then lets a failed assertion through; then checks nothing was left.
async fn with_cleanup(
    c: &ConnectionConfig,
    cleanup: &[&str],
    left: &str,
    body: impl Future<Output = ()>,
) {
    let result = AssertUnwindSafe(body).catch_unwind().await;
    let mut s = connect(c).await;
    for sql in cleanup {
        let mut out = QueryOutcome::default();
        if let Err(e) = s.execute(sql, 100, &mut out).await {
            eprintln!("cleanup {sql}: {e}");
        }
    }
    let n = scalar(&mut s, left).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert_eq!(n, "0", "objects left behind");
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
    FormValues {
        values,
        lists: Default::default(),
    }
}

fn set(v: &mut FormValues, k: &str, x: Value) {
    v.values.insert(k.into(), x);
}

async fn script(s: &mut Box<dyn Session>, id: Option<&str>, v: &FormValues) -> String {
    let script = s.scheduler_script("job", id, v).await.expect("script");
    eprintln!("--- {id:?}\n{script}");
    script
}

async fn save(s: &mut Box<dyn Session>, id: Option<&str>, v: &FormValues) -> String {
    let script = script(s, id, v).await;
    s.scheduler_save("job", id, v)
        .await
        .unwrap_or_else(|e| panic!("save: {e:?}\n{script}"))
        .expect("an id")
}

fn refused<T: std::fmt::Debug>(r: Result<T, Error>, needle: &str) {
    match r {
        Err(e) => assert!(e.to_string().contains(needle), "{e}"),
        Ok(v) => panic!("expected a refusal with «{needle}», got {v:?}"),
    }
}

// ---------------------------------------------------------------------------
// ClickHouse
// ---------------------------------------------------------------------------

const DB: &str = "dbine_test_sched_ch";
const DB2: &str = "dbine_test_sched_ch2";
const MV: &str = "dbine_test_sched_mv";
const MV2: &str = "dbine_test_sched_mv2";
const DEP: &str = "dbine_test_sched_dep";
const ORD: &str = "dbine_test_sched_ord";
const APP: &str = "dbine_test_sched_app";
const CH_CLEANUP: [&str; 6] = [
    "DROP DATABASE IF EXISTS dbine_test_sched_ch SYNC",
    "DROP DATABASE IF EXISTS dbine_test_sched_ch2 SYNC",
    "DROP DATABASE IF EXISTS dbine_test_sched_ord SYNC",
    "DROP USER IF EXISTS dbine_test_sched_ro",
    "DROP SETTINGS PROFILE IF EXISTS dbine_test_sched_ro",
    "DROP USER IF EXISTS dbine_test_sched_ql",
];
const CH_LEFT: &str =
    "SELECT (SELECT count() FROM system.databases WHERE startsWith(name, 'dbine_test_sched_'))
     + (SELECT count() FROM system.users WHERE startsWith(name, 'dbine_test_sched_'))
     + (SELECT count() FROM system.settings_profiles WHERE startsWith(name, 'dbine_test_sched_'))";

fn id(db: &str, name: &str) -> String {
    format!("`{db}`.`{name}`")
}

async fn create_query(s: &mut Box<dyn Session>, db: &str, name: &str) -> String {
    scalar(
        s,
        &format!("SELECT create_table_query FROM system.tables WHERE database = '{db}' AND name = '{name}'"),
    )
    .await
}

async fn status(s: &mut Box<dyn Session>, db: &str, view: &str) -> String {
    scalar(
        s,
        &format!(
            "SELECT status FROM system.view_refreshes WHERE database = '{db}' AND view = '{view}'"
        ),
    )
    .await
}

/// Waits until the view's status is `want` (a refresh running when the
/// view is stopped is cancelled in the background), or 15 s.
async fn wait_status(s: &mut Box<dyn Session>, db: &str, view: &str, want: &str) {
    for _ in 0..60 {
        if status(s, db, view).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(status(s, db, view).await, want, "{db}.{view}");
}

/// Waits for a successful refresh of the view (or 30 s).
async fn refreshed(s: &mut Box<dyn Session>, db: &str, view: &str) {
    for _ in 0..120 {
        let n = scalar(
            s,
            &format!("SELECT count() FROM system.view_refreshes WHERE database = '{db}' AND view = '{view}' AND last_success_time IS NOT NULL AND status != 'Running'"),
        )
        .await;
        if n == "1" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{db}.{view} never refreshed");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse_refreshable_views_managed_whole() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse") else {
        return;
    };
    let c2 = c.clone();
    with_cleanup(&c, &CH_CLEANUP, CH_LEFT, async move {
        let c = c2;
        let mut s = connect(&c).await;
        for sql in CH_CLEANUP {
            run(&mut s, sql).await;
        }
        run(
            &mut s,
            "CREATE DATABASE dbine_test_sched_ch;
             CREATE DATABASE dbine_test_sched_ch2;
             CREATE TABLE dbine_test_sched_ch.src (id UInt64) ENGINE = MergeTree ORDER BY id;
             INSERT INTO dbine_test_sched_ch.src SELECT number FROM numbers(10);
             CREATE TABLE dbine_test_sched_ch.dst (c UInt64) ENGINE = MergeTree ORDER BY c;",
        )
        .await;

        // What the scheduler has.
        let info = s.scheduler_info().await.expect("info");
        eprintln!("{info:?}");
        assert_eq!(info.kinds.len(), 1);
        let k = &info.kinds[0];
        assert_eq!((k.key.as_str(), k.label.as_str()), ("job", "Vistas refrescables"));
        assert!(k.can_create && k.can_edit && k.can_delete);
        assert!(info.logs, "the test user reads system.query_log");
        // Refreshes tagged with their view (25.4 on): a history to clear;
        // run and stop are the job's, no steps.
        assert!(k.clear_history && !k.run_from_step && k.actions.is_empty(), "{k:?}");

        // A new view, every option the form has.
        let form = s.scheduler_form("job", None).await.expect("form");
        assert!(form.choices["database"].iter().any(|(v, _)| v == DB));
        assert!(form.choices["table"].iter().any(|(v, _)| *v == id(DB, "dst")));
        let mut v = values_of(&form);
        set(&mut v, "database", json!(DB));
        set(&mut v, "name", json!(MV));
        set(&mut v, "interval", json!("1 HOUR"));
        set(&mut v, "offset", json!("5 MINUTE"));
        set(&mut v, "randomize", json!("1 MINUTE"));
        set(&mut v, "refresh_retries", json!(2));
        set(&mut v, "refresh_retry_initial_backoff_ms", json!(150));
        set(&mut v, "engine", json!("MergeTree ORDER BY c"));
        set(&mut v, "comment", json!("cada hora, it's"));
        // An unqualified name is read in the view's database (the script
        // says USE), not the connection's.
        set(&mut v, "query", json!("SELECT count() AS c FROM src;"));
        let sc = script(&mut s, None, &v).await;
        assert!(sc.contains("\nUSE `dbine_test_sched_ch`;\nCREATE MATERIALIZED VIEW"), "{sc}");
        let mv = save(&mut s, None, &v).await;
        assert_eq!(mv, id(DB, MV));
        let q = create_query(&mut s, DB, MV).await;
        assert!(
            q.contains("REFRESH EVERY 1 HOUR OFFSET 5 MINUTE RANDOMIZE FOR 1 MINUTE SETTINGS refresh_retries = 2, refresh_retry_initial_backoff_ms = 150"),
            "{q}"
        );
        assert!(q.contains("ENGINE = MergeTree ORDER BY c") && q.contains("cada hora, it"), "{q}");
        assert!(q.contains("DEFINER = dbine SQL SECURITY DEFINER"), "{q}");
        assert!(q.contains("FROM dbine_test_sched_ch.src"), "{q}");
        refreshed(&mut s, DB, MV).await;

        // An AFTER view that depends on it, appends to a table, stopped.
        let form = s.scheduler_form("job", None).await.expect("form");
        assert!(form.choices["refreshable"].iter().any(|(v, _)| *v == mv));
        let mut v = values_of(&form);
        set(&mut v, "database", json!(DB));
        set(&mut v, "name", json!(DEP));
        set(&mut v, "mode", json!("after"));
        set(&mut v, "interval", json!("30 SECOND"));
        set(&mut v, "depends_on", json!([mv]));
        set(&mut v, "target_mode", json!("to"));
        set(&mut v, "target", json!(id(DB, "dst")));
        set(&mut v, "append", json!(true));
        set(&mut v, "enabled", json!(false));
        set(&mut v, "query", json!("SELECT count() AS c FROM dbine_test_sched_ch.src"));
        let dep = save(&mut s, None, &v).await;
        let q = create_query(&mut s, DB, DEP).await;
        assert!(
            q.contains("REFRESH AFTER 30 SECOND DEPENDS ON dbine_test_sched_ch.dbine_test_sched_mv APPEND TO dbine_test_sched_ch.dst"),
            "{q}"
        );
        wait_status(&mut s, DB, DEP, "Disabled").await;

        // The tree lists both, with their state.
        let items = s.scheduler_items("job").await.expect("items");
        let item = |name: &str| {
            items
                .items
                .iter()
                .find(|i| i.name == format!("{DB}.{name}"))
                .unwrap_or_else(|| panic!("{name} in {items:?}"))
                .clone()
        };
        assert_eq!(item(MV).id, mv);
        assert_eq!(item(MV).enabled, Some(true));
        assert_eq!(item(DEP).enabled, Some(false));
        assert_eq!(item(DEP).description.as_deref(), Some("AFTER 30 SECOND"));

        // Read back as it was saved: nothing to change.
        let form = s.scheduler_form("job", Some(&mv)).await.expect("form");
        assert_eq!(form.values["offset"], json!("5 MINUTE"));
        assert_eq!(form.values["refresh_retries"], json!(2));
        assert_eq!(form.values["comment"], json!("cada hora, it's"));
        assert_eq!(form.values["definer"], json!("dbine"));
        let script_now = script(&mut s, Some(&mv), &values_of(&form)).await;
        assert!(script_now.contains("no hay cambios"), "{script_now}");

        // In place: the schedule, the query, the security and the comment.
        let mut v = values_of(&form);
        set(&mut v, "interval", json!("2 HOUR"));
        set(&mut v, "offset", json!(""));
        set(&mut v, "query", json!("SELECT count() + 1 AS c FROM dbine_test_sched_ch.src"));
        set(&mut v, "sql_security", json!("none"));
        set(&mut v, "comment", json!(""));
        let sc = script(&mut s, Some(&mv), &v).await;
        for part in [
            "ALTER TABLE `dbine_test_sched_ch`.`dbine_test_sched_mv` MODIFY QUERY\nSELECT count() + 1",
            "MODIFY REFRESH EVERY 2 HOUR RANDOMIZE FOR 1 MINUTE SETTINGS refresh_retries = 2, refresh_retry_initial_backoff_ms = 150;",
            "MODIFY SQL SECURITY NONE;",
            "MODIFY COMMENT '';",
        ] {
            assert!(sc.contains(part), "{part}\n{sc}");
        }
        assert!(!sc.contains("CREATE"), "{sc}");
        assert_eq!(save(&mut s, Some(&mv), &v).await, mv);
        let q = create_query(&mut s, DB, MV).await;
        assert!(q.contains("REFRESH EVERY 2 HOUR RANDOMIZE FOR 1 MINUTE SETTINGS"), "{q}");
        assert!(q.contains("SQL SECURITY NONE") && !q.contains("COMMENT"), "{q}");
        assert!(q.contains("count() + 1"), "{q}");
        let form = s.scheduler_form("job", Some(&mv)).await.expect("form");
        assert_eq!(form.values["interval"], json!("2 HOUR"));
        assert_eq!(form.values["sql_security"], json!("none"));
        assert!(script(&mut s, Some(&mv), &values_of(&form)).await.contains("no hay cambios"));

        // The appending view: its schedule (APPEND stays) and started.
        let form = s.scheduler_form("job", Some(&dep)).await.expect("form");
        assert_eq!(form.values["target_mode"], json!("to"));
        assert_eq!(form.values["depends_on"], json!([mv]));
        let mut v = values_of(&form);
        set(&mut v, "interval", json!("1 MINUTE"));
        set(&mut v, "enabled", json!(true));
        let sc = script(&mut s, Some(&dep), &v).await;
        assert!(sc.contains("MODIFY REFRESH AFTER 1 MINUTE DEPENDS ON `dbine_test_sched_ch`.`dbine_test_sched_mv` APPEND;"), "{sc}");
        assert!(sc.contains("SYSTEM START VIEW `dbine_test_sched_ch`.`dbine_test_sched_dep`;"), "{sc}");
        save(&mut s, Some(&dep), &v).await;
        assert_ne!(status(&mut s, DB, DEP).await, "Disabled");
        assert!(create_query(&mut s, DB, DEP).await.contains("REFRESH AFTER 1 MINUTE DEPENDS ON"));

        // What can't change in place: the view is created again, stopped.
        let form = s.scheduler_form("job", Some(&mv)).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "engine", json!("MergeTree ORDER BY tuple()"));
        set(&mut v, "enabled", json!(false));
        let sc = script(&mut s, Some(&mv), &v).await;
        assert!(sc.contains("CREATE OR REPLACE MATERIALIZED VIEW `dbine_test_sched_ch`.`dbine_test_sched_mv`"), "{sc}");
        assert!(sc.contains("-- ClickHouse no cambia en su lugar"), "{sc}");
        save(&mut s, Some(&mv), &v).await;
        let q = create_query(&mut s, DB, MV).await;
        assert!(q.contains("ORDER BY tuple()") && q.contains("REFRESH EVERY 2 HOUR"), "{q}");
        wait_status(&mut s, DB, MV, "Disabled").await;

        // Renamed into another database, and started.
        let form = s.scheduler_form("job", Some(&mv)).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "database", json!(DB2));
        set(&mut v, "name", json!(MV2));
        set(&mut v, "enabled", json!(true));
        let sc = script(&mut s, Some(&mv), &v).await;
        // With its own table it can't move: created there, this one dropped.
        assert!(sc.contains("CREATE MATERIALIZED VIEW `dbine_test_sched_ch2`.`dbine_test_sched_mv2`\n"), "{sc}");
        assert!(sc.ends_with("DROP VIEW `dbine_test_sched_ch`.`dbine_test_sched_mv`;\n"), "{sc}");
        let mv2 = save(&mut s, Some(&mv), &v).await;
        assert_eq!(mv2, id(DB2, MV2));
        assert_eq!(status(&mut s, DB, MV).await, "");
        assert_ne!(status(&mut s, DB2, MV2).await, "Disabled");
        // One writing to a table moves with RENAME (and its name with it).
        let form = s.scheduler_form("job", Some(&dep)).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "database", json!(DB2));
        set(&mut v, "name", json!("dbine_test_sched_dep2"));
        let sc = script(&mut s, Some(&dep), &v).await;
        assert!(sc.contains("RENAME TABLE `dbine_test_sched_ch`.`dbine_test_sched_dep` TO `dbine_test_sched_ch2`.`dbine_test_sched_dep2`;"), "{sc}");
        let dep2 = save(&mut s, Some(&dep), &v).await;
        assert_eq!(status(&mut s, DB, DEP).await, "");
        assert!(create_query(&mut s, DB2, "dbine_test_sched_dep2").await.contains("APPEND TO dbine_test_sched_ch.dst"));
        // And back.
        let form = s.scheduler_form("job", Some(&dep2)).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "database", json!(DB));
        set(&mut v, "name", json!(DEP));
        assert_eq!(save(&mut s, Some(&dep2), &v).await, dep);
        // A name that's taken is refused before anything runs.
        let form = s.scheduler_form("job", Some(&mv2)).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "database", json!(DB));
        set(&mut v, "name", json!("src"));
        refused(s.scheduler_script("job", Some(&mv2), &v).await, "Ya existe");

        // A view created outside DBine, its settings in another order than
        // DBine writes them, in a database without CREATE OR REPLACE
        // (Ordinary: only APPEND views refresh there), stopped.
        run(
            &mut s,
            "SET allow_deprecated_database_ordinary = 1;
             CREATE DATABASE dbine_test_sched_ord ENGINE = Ordinary;
             CREATE TABLE dbine_test_sched_ord.dst (c UInt64) ENGINE = MergeTree ORDER BY c;
             CREATE MATERIALIZED VIEW dbine_test_sched_ord.dbine_test_sched_app REFRESH EVERY 1 HOUR SETTINGS refresh_retry_initial_backoff_ms = 100, refresh_retries = 2 APPEND TO dbine_test_sched_ord.dst (c UInt64) AS SELECT 1 AS c;
             SYSTEM STOP VIEW dbine_test_sched_ord.dbine_test_sched_app;",
        )
        .await;
        let app = id(ORD, APP);
        wait_status(&mut s, ORD, APP, "Disabled").await;
        // A new comment alone leaves the refresh clause as it is.
        let form = s.scheduler_form("job", Some(&app)).await.expect("form");
        assert!(form.notes.iter().any(|n| n.contains("no admite CREATE OR REPLACE")), "{:?}", form.notes);
        let mut v = values_of(&form);
        set(&mut v, "comment", json!("solo el comentario"));
        let sc = script(&mut s, Some(&app), &v).await;
        assert!(sc.contains("MODIFY COMMENT 'solo el comentario';") && !sc.contains("MODIFY REFRESH"), "{sc}");
        save(&mut s, Some(&app), &v).await;
        // A new retries count, in the server's order.
        let form = s.scheduler_form("job", Some(&app)).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "refresh_retries", json!(3));
        let sc = script(&mut s, Some(&app), &v).await;
        assert!(sc.contains("MODIFY REFRESH EVERY 1 HOUR SETTINGS refresh_retry_initial_backoff_ms = 100, refresh_retries = 3 APPEND;"), "{sc}");
        save(&mut s, Some(&app), &v).await;
        // New columns the server refuses: dropped, refused, made again as
        // it was (stopped).
        let form = s.scheduler_form("job", Some(&app)).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "columns", json!("`c` NoSuchType"));
        let sc = script(&mut s, Some(&app), &v).await;
        assert!(sc.contains("DROP VIEW `dbine_test_sched_ord`.`dbine_test_sched_app`;\nCREATE MATERIALIZED VIEW"), "{sc}");
        assert!(sc.contains("-- Si el servidor rechaza la vista nueva, DBine vuelve a crear dbine_test_sched_ord.dbine_test_sched_app como estaba."), "{sc}");
        refused(
            s.scheduler_save("job", Some(&app), &v).await,
            "se volvió a crear «dbine_test_sched_ord.dbine_test_sched_app» como estaba",
        );
        let q = create_query(&mut s, ORD, APP).await;
        assert!(q.contains("REFRESH EVERY 1 HOUR SETTINGS refresh_retry_initial_backoff_ms = 100, refresh_retries = 3 APPEND TO dbine_test_sched_ord.dst"), "{q}");
        assert!(q.contains("`c` UInt64") && q.contains("solo el comentario"), "{q}");
        wait_status(&mut s, ORD, APP, "Disabled").await;
        s.scheduler_delete("job", &app).await.expect("delete");
        run(&mut s, "DROP DATABASE dbine_test_sched_ord SYNC").await;

        // Run, history, clear it.
        s.run_job(&mv2).await.expect("run");
        refreshed(&mut s, DB2, MV2).await;
        run(&mut s, "SYSTEM FLUSH LOGS").await;
        let count = "SELECT count() FROM system.query_log WHERE client_name = 'refreshable materialized view' AND log_comment = 'refresh of dbine_test_sched_ch2.dbine_test_sched_mv2'";
        assert_ne!(scalar(&mut s, count).await, "0");
        assert!(!s.job_runs(&mv2, 10).await.expect("runs").is_empty());
        s.clear_job_history(&mv2).await.expect("clear");
        assert_eq!(scalar(&mut s, count).await, "0");
        assert!(s.job_runs(&mv2, 10).await.expect("runs").is_empty());

        // The scheduler's log.
        let logs = s.scheduler_logs().await.expect("logs");
        assert_eq!(logs[0].key, "refreshes");
        assert_eq!(logs[0].columns[0], "Fecha (UTC)");
        eprintln!("{} refreshes, {} tables", logs[0].rows.len(), logs.len());

        // Refused before any SQL.
        let form = s.scheduler_form("job", None).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "database", json!(DB));
        set(&mut v, "name", json!("dbine_test_sched_bad"));
        set(&mut v, "query", json!("SELECT 1 AS c"));
        set(&mut v, "interval", json!("1h"));
        refused(s.scheduler_script("job", None, &v).await, "no es un intervalo");
        set(&mut v, "interval", json!("1 HOUR"));
        set(&mut v, "depends_on", json!([id(DB, "src")]));
        refused(s.scheduler_script("job", None, &v).await, "no es una vista refrescable");
        set(&mut v, "depends_on", json!([]));
        set(&mut v, "engine", json!("Memory; DROP DATABASE dbine_test_sched_ch"));
        refused(s.scheduler_save("job", None, &v).await, "«Motor»");
        // A name with quotes is only a name.
        let odd = "dbine_test_sched_x`; DROP DATABASE dbine_test_sched_ch; --";
        set(&mut v, "engine", json!("Memory"));
        set(&mut v, "name", json!(odd));
        let odd_id = save(&mut s, None, &v).await;
        assert_eq!(scalar(&mut s, "SELECT count() FROM system.databases WHERE name = 'dbine_test_sched_ch'").await, "1");
        assert!(s.scheduler_items("job").await.unwrap().items.iter().any(|i| i.id == odd_id));
        s.scheduler_delete("job", &odd_id).await.expect("delete");
        // Ids and kinds that aren't the scheduler's.
        for bad in ["x' OR 1=1 --", "`dbine_test_sched_ch`.`a`; DROP DATABASE dbine_test_sched_ch", "dbine_test_sched_ch.dbine_test_sched_dep"] {
            refused(s.scheduler_form("job", Some(bad)).await, "no es un id");
            refused(s.scheduler_delete("job", bad).await, "no es un id");
        }
        refused(s.scheduler_delete("job", &id(DB, "src")).await, "no existe la vista refrescable");
        refused(s.scheduler_items("schedule").await, "no es un tipo");
        refused(s.run_job_from(&dep, "x").await, "paso");

        // A user whose profile is read-only sees the views, and nothing
        // in the designer can be saved.
        run(
            &mut s,
            "CREATE SETTINGS PROFILE dbine_test_sched_ro SETTINGS readonly = 1;
             CREATE USER dbine_test_sched_ro IDENTIFIED BY 'Pw_sched_ro1' SETTINGS PROFILE 'dbine_test_sched_ro';
             GRANT SELECT ON *.* TO dbine_test_sched_ro;",
        )
        .await;
        let mut ro = connect(&ConnectionConfig {
            username: Some("dbine_test_sched_ro".into()),
            password: Some("Pw_sched_ro1".into()),
            ..c.clone()
        })
        .await;
        let info = ro.scheduler_info().await.expect("info");
        assert!(!info.kinds[0].can_create && !info.kinds[0].can_edit && !info.kinds[0].can_delete);
        assert!(!info.kinds[0].clear_history, "a readonly user deletes nothing");
        assert!(info.notes.iter().any(|n| n.contains("readonly")), "{info:?}");
        let form = ro.scheduler_form("job", Some(&dep)).await.expect("form");
        assert!(form.read_only && form.notes[0].contains("readonly"), "{:?}", form.notes);
        refused(ro.scheduler_save("job", Some(&dep), &values_of(&form)).await, "readonly");
        refused(ro.scheduler_delete("job", &dep).await, "readonly");
        drop(ro);
        run(&mut s, "DROP USER dbine_test_sched_ro; DROP SETTINGS PROFILE dbine_test_sched_ro").await;

        // A user who reads system.query_log but can't delete from it (no
        // ALTER DELETE), with a profile that isn't read-only, isn't offered
        // to clear the views' history.
        run(
            &mut s,
            "CREATE USER dbine_test_sched_ql IDENTIFIED BY 'Pw_sched_ql1';
             GRANT SELECT ON *.* TO dbine_test_sched_ql;
             GRANT SELECT ON system.query_log TO dbine_test_sched_ql;",
        )
        .await;
        let mut ql = connect(&ConnectionConfig {
            username: Some("dbine_test_sched_ql".into()),
            password: Some("Pw_sched_ql1".into()),
            ..c.clone()
        })
        .await;
        let info = ql.scheduler_info().await.expect("info");
        assert!(info.logs, "it reads system.query_log: {info:?}");
        assert!(info.kinds[0].can_edit, "{info:?}");
        assert!(!info.kinds[0].clear_history, "no ALTER DELETE on system.query_log");
        drop(ql);
        run(&mut s, "DROP USER dbine_test_sched_ql").await;
        // A read-only connection of a user who holds it keeps the option
        // (DBine's mode refuses the clearing itself).
        let mut ro = connect(&ConnectionConfig { read_only: true, ..c.clone() }).await;
        assert!(ro.scheduler_info().await.expect("info").kinds[0].clear_history);
        drop(ro);

        // A read-only connection shows and scripts, and changes nothing.
        let mut ro = ReadOnlySession::new(connect(&ConnectionConfig { read_only: true, ..c.clone() }).await);
        let form = ro.scheduler_form("job", Some(&dep)).await.expect("form");
        assert!(form.read_only);
        let v = values_of(&form);
        assert!(ro.scheduler_script("job", Some(&dep), &v).await.is_ok());
        assert!(ro.scheduler_delete_script("job", &dep).await.is_ok());
        assert!(ro.scheduler_save("job", Some(&dep), &v).await.is_err());
        assert!(ro.scheduler_delete("job", &dep).await.is_err());
        assert!(ro.clear_job_history(&dep).await.is_err());

        // Deleted: the target table stays.
        let sc = s.scheduler_delete_script("job", &dep).await.expect("script");
        assert!(sc.contains("DROP VIEW `dbine_test_sched_ch`.`dbine_test_sched_dep`;"), "{sc}");
        assert!(sc.contains("La tabla destino dbine_test_sched_ch.dst queda"), "{sc}");
        s.scheduler_delete("job", &dep).await.expect("delete");
        s.scheduler_delete("job", &mv2).await.expect("delete");
        assert_eq!(
            scalar(&mut s, "SELECT count() FROM system.view_refreshes WHERE startsWith(database, 'dbine_test_sched_')").await,
            "0"
        );
        assert_eq!(scalar(&mut s, "SELECT count() FROM system.tables WHERE database = 'dbine_test_sched_ch' AND name = 'dst'").await, "1");
    })
    .await;
}

/// 22.8 has no refreshable views: no kinds, a note that says why, and the
/// rest refused as unsupported. Nothing is created.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse_22_8_has_no_scheduler() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_22_URL", "clickhouse") else {
        return;
    };
    let mut s = connect(&c).await;
    let info = s.scheduler_info().await.expect("info");
    assert!(info.kinds.is_empty() && !info.logs, "{info:?}");
    assert!(info.notes[0].contains("23.12"), "{info:?}");
    let items = s.scheduler_items("job").await.expect("items");
    assert!(
        items.items.is_empty() && items.notes[0].contains("23.12"),
        "{items:?}"
    );
    assert!(matches!(
        s.scheduler_form("job", None).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        s.scheduler_script("job", None, &FormValues::default())
            .await,
        Err(Error::Unsupported(_))
    ));
}

// ---------------------------------------------------------------------------
// Timeplus Proton
// ---------------------------------------------------------------------------

const T1: &str = "dbine_test_sched_t1";
const T2: &str = "dbine_test_sched_t2";
const T3: &str = "dbine_test_sched_t3";
const TP_CLEANUP: [&str; 4] = [
    "DROP TASK IF EXISTS default.dbine_test_sched_t1",
    "DROP TASK IF EXISTS default.dbine_test_sched_t2",
    "DROP TASK IF EXISTS default.dbine_test_sched_t3",
    "DROP STREAM IF EXISTS default.dbine_test_sched_out",
];
const TP_LEFT: &str = "SELECT count() FROM system.tables WHERE database = 'default' AND name LIKE 'dbine_test_sched%'";

/// A task's row of `SHOW TASKS … verbose` (name, uuid, version, status…).
async fn task_row(s: &mut Box<dyn Session>, name: &str) -> Option<Vec<Value>> {
    rows(s, "SHOW TASKS FROM default SETTINGS verbose = 1")
        .await
        .into_iter()
        .find(|r| r.first() == Some(&json!(name)))
}

async fn show_task(s: &mut Box<dyn Session>, name: &str) -> String {
    scalar(s, &format!("SHOW CREATE TASK default.{name}")).await
}

async fn tasks_left(s: &mut Box<dyn Session>) -> usize {
    rows(s, "SHOW TASKS FROM default")
        .await
        .iter()
        .filter(|r| {
            r.first()
                .and_then(Value::as_str)
                .is_some_and(|n| n.starts_with("dbine_test_sched_"))
        })
        .count()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn proton_tasks_managed_whole() {
    let Some(c) = cfg("DBINE_TEST_TIMEPLUS_URL", "timeplus") else {
        return;
    };
    let c2 = c.clone();
    with_cleanup(&c, &TP_CLEANUP, TP_LEFT, async move {
        let c = c2;
        let mut s = connect(&c).await;
        for sql in TP_CLEANUP {
            run(&mut s, sql).await;
        }
        run(
            &mut s,
            "CREATE STREAM default.dbine_test_sched_out (n uint64)",
        )
        .await;

        let info = s.scheduler_info().await.expect("info");
        eprintln!("{info:?}");
        assert_eq!(info.kinds.len(), 1);
        assert_eq!(
            (info.kinds[0].key.as_str(), info.kinds[0].label.as_str()),
            ("job", "Tareas")
        );
        assert!(info.kinds[0].can_create && info.kinds[0].can_edit && info.kinds[0].can_delete);
        assert!(!info.logs);
        // Proton keeps only the last run: nothing to clear, no steps, and
        // "run now" is the job's EXECUTE TASK.
        let k = &info.kinds[0];
        assert!(
            !k.clear_history && !k.run_from_step && k.actions.is_empty(),
            "{k:?}"
        );

        // A task writing into a stream.
        let form = s.scheduler_form("job", None).await.expect("form");
        let out = "`default`.`dbine_test_sched_out`";
        assert!(
            form.choices["stream"].iter().any(|(v, _)| v == out),
            "{:?}",
            form.choices
        );
        let mut v = values_of(&form);
        set(&mut v, "database", json!("default"));
        set(&mut v, "name", json!(T1));
        set(&mut v, "schedule", json!("1h"));
        set(&mut v, "timeout", json!("30m"));
        set(&mut v, "into", json!(out));
        set(
            &mut v,
            "query",
            json!("SELECT count() AS n FROM numbers(3)"),
        );
        let t1 = save(&mut s, None, &v).await;
        assert_eq!(t1, "`default`.`dbine_test_sched_t1`");
        let show = show_task(&mut s, T1).await;
        assert!(
            show.contains("SCHEDULE 1h") && show.contains("TIMEOUT 30m"),
            "{show}"
        );
        assert!(show.contains("INTO default.dbine_test_sched_out"), "{show}");

        // A paused one, without a target.
        let mut v = values_of(&form);
        set(&mut v, "database", json!("default"));
        set(&mut v, "name", json!(T2));
        set(&mut v, "schedule", json!("5 minutes"));
        set(&mut v, "enabled", json!(false));
        set(&mut v, "query", json!("SELECT 1 AS n"));
        let t2 = save(&mut s, None, &v).await;
        assert!(show_task(&mut s, T2).await.contains("SCHEDULE 5m"));
        assert_eq!(task_row(&mut s, T2).await.unwrap()[3], json!(1), "paused");

        let items = s.scheduler_items("job").await.expect("items");
        let item = |name: &str| {
            items
                .items
                .iter()
                .find(|i| i.name == format!("default.{name}"))
                .unwrap_or_else(|| panic!("{name} in {items:?}"))
                .clone()
        };
        assert_eq!(item(T1).id, t1);
        assert_eq!(item(T2).enabled, Some(false));

        // Read back: nothing to change.
        let form = s.scheduler_form("job", Some(&t1)).await.expect("form");
        assert_eq!(form.values["into"], json!(out));
        assert_eq!(form.values["timeout"], json!("30m"));
        assert!(script(&mut s, Some(&t1), &values_of(&form))
            .await
            .contains("no hay cambios"));

        // Changed in place: same UUID.
        let uuid = task_row(&mut s, T1).await.unwrap()[1].clone();
        let mut v = values_of(&form);
        set(&mut v, "schedule", json!("2h"));
        let sc = script(&mut s, Some(&t1), &v).await;
        assert!(
            sc.contains("CREATE OR REPLACE TASK `default`.`dbine_test_sched_t1`\nSCHEDULE 2h"),
            "{sc}"
        );
        save(&mut s, Some(&t1), &v).await;
        assert!(show_task(&mut s, T1).await.contains("SCHEDULE 2h"));
        assert_eq!(task_row(&mut s, T1).await.unwrap()[1], uuid);

        // A paused task changed stays paused.
        let form = s.scheduler_form("job", Some(&t2)).await.expect("form");
        assert_eq!(form.values["enabled"], json!(false));
        let mut v = values_of(&form);
        set(&mut v, "timeout", json!("10s"));
        let sc = script(&mut s, Some(&t2), &v).await;
        assert!(
            sc.contains("SYSTEM PAUSE TASK `default`.`dbine_test_sched_t2`;"),
            "{sc}"
        );
        save(&mut s, Some(&t2), &v).await;
        assert!(show_task(&mut s, T2).await.contains("TIMEOUT 10s"));
        assert_eq!(
            task_row(&mut s, T2).await.unwrap()[3],
            json!(1),
            "still paused"
        );
        // Unpaused alone.
        set(&mut v, "enabled", json!(true));
        let sc = script(&mut s, Some(&t2), &v).await;
        assert!(
            sc.ends_with("SYSTEM UNPAUSE TASK `default`.`dbine_test_sched_t2`;\n"),
            "{sc}"
        );
        save(&mut s, Some(&t2), &v).await;
        assert_eq!(task_row(&mut s, T2).await.unwrap()[3], json!(0));

        // Renamed: a new task, the old one dropped.
        set(&mut v, "name", json!(T3));
        let sc = script(&mut s, Some(&t2), &v).await;
        assert!(sc.contains("no renombra"), "{sc}");
        let t3 = save(&mut s, Some(&t2), &v).await;
        assert_eq!(t3, "`default`.`dbine_test_sched_t3`");
        assert!(task_row(&mut s, T2).await.is_none());
        assert!(show_task(&mut s, T3).await.contains("TIMEOUT 10s"));

        // Cron: in the grammar, refused by Proton 3.0; the task is untouched.
        let form = s.scheduler_form("job", Some(&t1)).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "schedule_mode", json!("cron"));
        set(&mut v, "cron", json!("0 * * * *"));
        assert!(script(&mut s, Some(&t1), &v)
            .await
            .contains("SCHEDULE '0 * * * *'"));
        if let Err(e) = s.scheduler_save("job", Some(&t1), &v).await {
            eprintln!("cron refused: {e}");
            assert!(show_task(&mut s, T1).await.contains("SCHEDULE 2h"));
        }

        // What Proton doesn't have.
        assert!(matches!(
            s.clear_job_history(&t1).await,
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            s.scheduler_logs().await,
            Err(Error::Unsupported(_))
        ));
        assert!(s.run_job_from(&t1, "x").await.is_err());
        for bad in [
            "x' OR 1=1 --",
            "`default`.`t`; DROP TASK x",
            "default.dbine_test_sched_t1",
        ] {
            refused(s.scheduler_form("job", Some(bad)).await, "no es un id");
            refused(s.scheduler_delete("job", bad).await, "no es un id");
        }
        refused(
            s.scheduler_delete("job", "`default`.`dbine_test_sched_out`")
                .await,
            "no existe la tarea",
        );

        // Read-only.
        let mut ro = ReadOnlySession::new(
            connect(&ConnectionConfig {
                read_only: true,
                ..c.clone()
            })
            .await,
        );
        let form = ro.scheduler_form("job", Some(&t1)).await.expect("form");
        assert!(form.read_only);
        assert!(ro
            .scheduler_script("job", Some(&t1), &values_of(&form))
            .await
            .is_ok());
        assert!(ro
            .scheduler_save("job", Some(&t1), &values_of(&form))
            .await
            .is_err());
        assert!(ro.scheduler_delete("job", &t1).await.is_err());

        // Deleted.
        let sc = s.scheduler_delete_script("job", &t1).await.expect("script");
        assert!(
            sc.ends_with("DROP TASK `default`.`dbine_test_sched_t1`;\n"),
            "{sc}"
        );
        s.scheduler_delete("job", &t1).await.expect("delete");
        s.scheduler_delete("job", &t3).await.expect("delete");
        assert_eq!(tasks_left(&mut s).await, 0);
    })
    .await;
    let mut s = connect(&c).await;
    assert_eq!(tasks_left(&mut s).await, 0, "tasks left behind");
}

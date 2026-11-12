//! Server jobs against real servers: refreshable materialized views on
//! ClickHouse, nothing on 22.8 (before them), and tasks on Timeplus Proton.
//! Skipped without the URLs:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_CLICKHOUSE_22_URL=http://dbine:dbine@localhost:25124 \
//! DBINE_TEST_CLICKHOUSE_24_URL=http://dbine:dbine@localhost:25125 \
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119 \
//!   cargo test -p dbine-driver-clickhouse --test jobs -- --ignored
//! ```
//!
//! The 24.8 server (refreshable views without the per-view tag in
//! `query_log`):
//!
//! ```sh
//! docker run -d --name dbine-test-jobs-clickhouse-24 -p 25125:8123 \
//!   -e CLICKHOUSE_USER=dbine -e CLICKHOUSE_PASSWORD=dbine \
//!   -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 clickhouse/clickhouse-server:24.8
//! ```
//!
//! Each test makes its own `dbine_test_jobs_*` objects and drops them at the
//! end, also when an assertion fails.

use dbine_driver::{ConnectionConfig, JobOutcome, QueryOutcome, ServerJob, Session};
use std::time::Duration;

fn cfg(env: &str, id: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
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
    assert!(d.capabilities().jobs);
    d.connect(c, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn find(s: &mut Box<dyn Session>, name: &str) -> Option<ServerJob> {
    s.jobs()
        .await
        .unwrap()
        .jobs
        .into_iter()
        .find(|j| j.name == name)
}

/// Waits until `f` holds for the job (or 20 s).
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

/// Malicious or foreign ids are refused before any SQL, and the database is
/// still there after them.
async fn refuses_bad_ids(s: &mut Box<dyn Session>, db: &str) {
    for bad in [
        "x' OR 1=1 --",
        "`dbine_test_jobs_ch`.`a`; DROP DATABASE dbine_test_jobs_ch",
        "`dbine_test_jobs_ch`.`a``b`",
        "dbine_test_jobs_ch.dbine_test_jobs_mv",
    ] {
        assert!(s.job_detail(bad).await.is_err(), "{bad}");
        assert!(s.run_job(bad).await.is_err(), "{bad}");
        assert!(s.set_job_enabled(bad, false).await.is_err(), "{bad}");
        assert!(s.job_script(bad).await.is_err(), "{bad}");
    }
    // The shape is right; the name is just text in a parameter.
    let quoted = format!("`{db}`.`x'; DROP DATABASE {db}; --`");
    let e = s.run_job(&quoted).await.unwrap_err();
    assert!(e.to_string().contains("no existe"), "{e}");
    assert!(s.list_databases().await.unwrap().iter().any(|d| d == db));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse_refreshable_views() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse") else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let mut s = connect(&c).await;
    run(&mut s, "DROP DATABASE IF EXISTS dbine_test_jobs_ch").await;
    let body = tokio::spawn(async move {
        clickhouse_body(&mut s).await;
    })
    .await;
    let mut s = connect(&c).await;
    run(&mut s, "DROP DATABASE IF EXISTS dbine_test_jobs_ch").await;
    body.unwrap();
}

async fn clickhouse_body(s: &mut Box<dyn Session>) {
    run(
        s,
        "CREATE DATABASE dbine_test_jobs_ch;
         CREATE TABLE dbine_test_jobs_ch.src (id UInt64) ENGINE = MergeTree ORDER BY id;
         INSERT INTO dbine_test_jobs_ch.src SELECT number FROM numbers(10);
         CREATE MATERIALIZED VIEW dbine_test_jobs_ch.dbine_test_jobs_mv REFRESH EVERY 1 HOUR
           ENGINE = MergeTree ORDER BY c COMMENT 'cada hora'
           AS SELECT count() AS c FROM dbine_test_jobs_ch.src;
         CREATE TABLE dbine_test_jobs_ch.dst (c UInt64) ENGINE = MergeTree ORDER BY c;
         CREATE MATERIALIZED VIEW dbine_test_jobs_ch.dbine_test_jobs_append
           REFRESH AFTER 30 MINUTE APPEND TO dbine_test_jobs_ch.dst
           AS SELECT count() AS c FROM dbine_test_jobs_ch.src;",
    )
    .await;

    // The list: both views, with their schedule and what they allow.
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    let mv = wait_for(s, "dbine_test_jobs_mv", |j| {
        j.last_outcome == Some(JobOutcome::Succeeded)
    })
    .await;
    assert_eq!(mv.id, "`dbine_test_jobs_ch`.`dbine_test_jobs_mv`");
    assert_eq!(mv.group.as_deref(), Some("dbine_test_jobs_ch"));
    assert_eq!(mv.schedule.as_deref(), Some("EVERY 1 HOUR"));
    assert_eq!(mv.description.as_deref(), Some("cada hora"));
    assert!(mv.enabled && mv.can_run && mv.can_stop && mv.can_enable && mv.has_script);
    assert!(mv.has_history, "the test user reads system.query_log");
    assert!(mv.next_run.as_deref().is_some_and(|t| t.ends_with('Z')));
    assert!(mv.last_started.as_deref().is_some_and(|t| t.ends_with('Z')));
    let append = find(s, "dbine_test_jobs_append").await.unwrap();
    assert_eq!(append.schedule.as_deref(), Some("AFTER 30 MINUTE"));

    // The detail.
    let d = s.job_detail(&mv.id).await.unwrap();
    assert_eq!(d.job.id, mv.id);
    assert_eq!(d.steps.len(), 1);
    assert!(d.steps[0]
        .command
        .as_deref()
        .unwrap_or("")
        .contains("count()"));
    let d = s.job_detail(&append.id).await.unwrap();
    assert_eq!(d.steps[0].target.as_deref(), Some("dbine_test_jobs_ch.dst"));
    assert!(d
        .properties
        .iter()
        .any(|p| p.label == "Al refrescar" && p.value.contains("APPEND")));

    // Run now, and the run shows in the history.
    let before = s.job_runs(&mv.id, 50).await.unwrap().len();
    s.run_job(&mv.id).await.unwrap();
    run(s, "SYSTEM FLUSH LOGS").await;
    let mut runs = Vec::new();
    for _ in 0..40 {
        runs = s.job_runs(&mv.id, 50).await.unwrap();
        if runs.len() > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        run(s, "SYSTEM FLUSH LOGS").await;
    }
    eprintln!("{runs:#?}");
    assert!(runs.len() > before, "the run is in the history");
    assert_eq!(runs[0].outcome, JobOutcome::Succeeded);
    assert!(runs[0].started.as_deref().is_some_and(|t| t.ends_with('Z')));
    assert!(
        runs.windows(2).all(|w| w[0].started >= w[1].started),
        "newest first"
    );
    assert_eq!(s.job_runs(&mv.id, 1).await.unwrap().len(), 1);

    // A view that reads this one: its refreshes list this view among their
    // tables, but they're its own history, not this one's.
    run(s, "SYSTEM FLUSH LOGS").await;
    let mv_before = s.job_runs(&mv.id, 50).await.unwrap();
    run(
        s,
        "CREATE MATERIALIZED VIEW dbine_test_jobs_ch.dbine_test_jobs_reader REFRESH EVERY 1 HOUR
           ENGINE = MergeTree ORDER BY c
           AS SELECT c FROM dbine_test_jobs_ch.dbine_test_jobs_mv",
    )
    .await;
    let reader = wait_for(s, "dbine_test_jobs_reader", |j| {
        j.last_outcome == Some(JobOutcome::Succeeded)
    })
    .await;
    s.run_job(&reader.id).await.unwrap();
    let mut reader_runs = Vec::new();
    for _ in 0..40 {
        run(s, "SYSTEM FLUSH LOGS").await;
        reader_runs = s.job_runs(&reader.id, 50).await.unwrap();
        if reader_runs.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(reader_runs.len() >= 2, "{reader_runs:#?}");
    let mv_after = s.job_runs(&mv.id, 50).await.unwrap();
    let ids = |runs: &[dbine_driver::JobRun]| runs.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    assert_eq!(
        ids(&mv_after),
        ids(&mv_before),
        "only the view's own refreshes"
    );
    assert!(mv_after
        .iter()
        .all(|r| !reader_runs.iter().any(|o| o.id == r.id)));

    // Disable and enable.
    s.set_job_enabled(&mv.id, false).await.unwrap();
    let off = find(s, "dbine_test_jobs_mv").await.unwrap();
    assert!(!off.enabled && !off.can_run && off.next_run.is_none());
    assert!(s.run_job(&mv.id).await.is_err(), "a stopped view isn't run");
    s.set_job_enabled(&mv.id, true).await.unwrap();
    let on = find(s, "dbine_test_jobs_mv").await.unwrap();
    assert!(on.enabled && on.can_run);

    // Stop a running refresh.
    run(
        s,
        "CREATE MATERIALIZED VIEW dbine_test_jobs_ch.dbine_test_jobs_slow REFRESH EVERY 1 YEAR
           ENGINE = Memory EMPTY
           AS SELECT count() AS c FROM numbers(100000000000) WHERE NOT ignore(sipHash64(number))",
    )
    .await;
    let slow = find(s, "dbine_test_jobs_slow").await.unwrap();
    s.run_job(&slow.id).await.unwrap();
    let going = wait_for(s, "dbine_test_jobs_slow", |j| j.running).await;
    assert_eq!(going.last_outcome, Some(JobOutcome::Running));
    let runs = s.job_runs(&slow.id, 10).await.unwrap();
    assert_eq!(runs[0].outcome, JobOutcome::Running);
    s.stop_job(&slow.id).await.unwrap();
    let stopped = wait_for(s, "dbine_test_jobs_slow", |j| !j.running).await;
    assert_eq!(stopped.last_outcome, Some(JobOutcome::Canceled));

    // The script recreates the view as it is.
    let script = s.job_script(&mv.id).await.unwrap();
    eprintln!("{script}");
    assert!(script.starts_with("CREATE MATERIALIZED VIEW"), "{script}");
    assert!(script.contains("REFRESH EVERY 1 HOUR"), "{script}");
    run(s, "DROP VIEW dbine_test_jobs_ch.dbine_test_jobs_mv").await;
    assert!(find(s, "dbine_test_jobs_mv").await.is_none());
    run(s, &script).await;
    let again = find(s, "dbine_test_jobs_mv").await.unwrap();
    assert_eq!(again.schedule.as_deref(), Some("EVERY 1 HOUR"));

    refuses_bad_ids(s, "dbine_test_jobs_ch").await;
}

/// 24.8 has refreshable views, but its `query_log` doesn't say which view
/// a refresh belongs to: they're listed without history, and a note says
/// why.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse_24_8_without_history() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_24_URL", "clickhouse") else {
        eprintln!("DBINE_TEST_CLICKHOUSE_24_URL not set; skipping");
        return;
    };
    let mut s = connect(&c).await;
    run(&mut s, "DROP DATABASE IF EXISTS dbine_test_jobs_ch24").await;
    let body = tokio::spawn(async move {
        let s = &mut s;
        run(
            s,
            "CREATE DATABASE dbine_test_jobs_ch24;
             SET allow_experimental_refreshable_materialized_view = 1;
             CREATE MATERIALIZED VIEW dbine_test_jobs_ch24.dbine_test_jobs_mv REFRESH EVERY 1 HOUR
               ENGINE = MergeTree ORDER BY c AS SELECT count() AS c FROM numbers(10);",
        )
        .await;
        let mv = wait_for(s, "dbine_test_jobs_mv", |j| {
            j.last_outcome == Some(JobOutcome::Succeeded)
        })
        .await;
        assert!(!mv.has_history && mv.can_run && mv.can_enable && mv.has_script);
        let list = s.jobs().await.unwrap();
        assert!(list.notes.iter().any(|n| n.contains("25.4")), "{list:?}");
        let e = s.job_runs(&mv.id, 10).await.unwrap_err();
        assert!(e.to_string().contains("25.4"), "{e}");
        assert!(!s.job_detail(&mv.id).await.unwrap().job.has_history);
    })
    .await;
    let mut s = connect(&c).await;
    run(&mut s, "DROP DATABASE IF EXISTS dbine_test_jobs_ch24").await;
    body.unwrap();
}

/// 22.8 has no refreshable views: an empty list that says why.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse_22_8_has_none() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_22_URL", "clickhouse") else {
        eprintln!("DBINE_TEST_CLICKHOUSE_22_URL not set; skipping");
        return;
    };
    let mut s = connect(&c).await;
    let list = s.jobs().await.unwrap();
    assert!(list.jobs.is_empty());
    assert!(list.notes[0].contains("23.12"), "{list:?}");
    assert!(s
        .job_detail("`default`.`dbine_test_jobs_mv`")
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timeplus_tasks() {
    let Some(c) = cfg("DBINE_TEST_TIMEPLUS_URL", "timeplus") else {
        eprintln!("DBINE_TEST_TIMEPLUS_URL not set; skipping");
        return;
    };
    let mut s = connect(&c).await;
    run(&mut s, "DROP TASK IF EXISTS default.dbine_test_jobs_task").await;
    let body = tokio::spawn(async move {
        timeplus_body(&mut s).await;
    })
    .await;
    let mut s = connect(&c).await;
    run(&mut s, "DROP TASK IF EXISTS default.dbine_test_jobs_task").await;
    body.unwrap();
}

async fn timeplus_body(s: &mut Box<dyn Session>) {
    run(
        s,
        "CREATE TASK default.dbine_test_jobs_task SCHEDULE 1h TIMEOUT 30s AS SELECT 1 AS x",
    )
    .await;
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    let t = find(s, "dbine_test_jobs_task").await.unwrap();
    assert_eq!(t.id, "`default`.`dbine_test_jobs_task`");
    assert_eq!(t.kind, "Task");
    assert_eq!(t.schedule.as_deref(), Some("1h"));
    assert!(t.enabled && t.can_run && t.can_enable && t.has_script);
    assert!(!t.can_stop && !t.has_history);

    let d = s.job_detail(&t.id).await.unwrap();
    assert_eq!(d.steps[0].command.as_deref(), Some("SELECT 1 AS x"));
    assert!(d
        .properties
        .iter()
        .any(|p| p.label == "Tiempo máximo (TIMEOUT)" && p.value == "30s"));

    // Run now: the last run shows in the list.
    s.run_job(&t.id).await.unwrap();
    let ran = wait_for(s, "dbine_test_jobs_task", |j| {
        j.last_outcome == Some(JobOutcome::Succeeded)
    })
    .await;
    assert!(ran
        .last_started
        .as_deref()
        .is_some_and(|t| t.ends_with('Z')));
    assert!(s.job_runs(&t.id, 10).await.is_err(), "no history");

    // Disable (pause) and enable.
    s.set_job_enabled(&t.id, false).await.unwrap();
    let off = find(s, "dbine_test_jobs_task").await.unwrap();
    assert!(!off.enabled && !off.can_run);
    assert!(s.run_job(&t.id).await.is_err(), "a paused task isn't run");
    s.set_job_enabled(&t.id, true).await.unwrap();
    assert!(find(s, "dbine_test_jobs_task").await.unwrap().enabled);

    // The script recreates it.
    let script = s.job_script(&t.id).await.unwrap();
    eprintln!("{script}");
    assert!(script.contains("SCHEDULE 1h"), "{script}");
    run(s, "DROP TASK default.dbine_test_jobs_task").await;
    assert!(find(s, "dbine_test_jobs_task").await.is_none());
    run(s, &script).await;
    assert!(find(s, "dbine_test_jobs_task").await.is_some());

    for bad in ["x' OR 1=1 --", "`default`.`t`; DROP TASK x", "default.t"] {
        assert!(s.run_job(bad).await.is_err(), "{bad}");
        assert!(s.job_script(bad).await.is_err(), "{bad}");
    }
    let e = s
        .run_job("`default`.`x'; DROP TASK dbine_test_jobs_task; --`")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no existe"), "{e}");
    assert!(find(s, "dbine_test_jobs_task").await.is_some());
}

/// A read-only connection lists the tasks with their state (the `verbose`
/// setting goes on the request, which `readonly = 1` accepts), says that
/// the last run can't be read there, and can't change them.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timeplus_tasks_read_only() {
    let Some(c) = cfg("DBINE_TEST_TIMEPLUS_URL", "timeplus") else {
        eprintln!("DBINE_TEST_TIMEPLUS_URL not set; skipping");
        return;
    };
    let mut s = connect(&c).await;
    run(&mut s, "DROP TASK IF EXISTS default.dbine_test_jobs_ro").await;
    let rw = c.clone();
    let body = tokio::spawn(async move {
        let s = &mut s;
        run(
            s,
            "CREATE TASK default.dbine_test_jobs_ro SCHEDULE 1h TIMEOUT 30s AS SELECT 1 AS x",
        )
        .await;
        run(s, "SYSTEM PAUSE TASK default.dbine_test_jobs_ro").await;
        let ro = ConnectionConfig {
            read_only: true,
            ..rw
        };
        let mut r = connect(&ro).await;
        let list = r.jobs().await.unwrap();
        eprintln!("{list:#?}");
        let t = list
            .jobs
            .iter()
            .find(|j| j.name == "dbine_test_jobs_ro")
            .unwrap();
        assert!(!t.enabled, "the paused state is read");
        assert!(
            list.notes.iter().any(|n| n.contains("solo lectura")),
            "{list:?}"
        );
        let d = r.job_detail(&t.id).await.unwrap();
        assert_eq!(d.steps[0].command.as_deref(), Some("SELECT 1 AS x"));
        assert!(r.set_job_enabled(&t.id, true).await.is_err());
        assert!(r.job_script(&t.id).await.unwrap().contains("SCHEDULE 1h"));
    })
    .await;
    let mut s = connect(&c).await;
    run(&mut s, "DROP TASK IF EXISTS default.dbine_test_jobs_ro").await;
    body.unwrap();
}

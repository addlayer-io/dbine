//! Server jobs against real servers: MySQL, MariaDB and OceanBase events, StarRocks
//! tasks and Doris jobs, with the refresh jobs of materialized views. Each
//! test reads `DBINE_TEST_<ENGINE>_URL` (`mysql://user:pass@host:port`, see
//! tests/integration.rs) and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_OCEANBASE_URL=mysql://root@test:pw@localhost:25035 \
//! DBINE_TEST_STARROCKS_URL=mysql://root@localhost:25030 \
//! DBINE_TEST_DORIS_URL=mysql://root@localhost:25031 \
//!   cargo test -p dbine-driver-mysql --test jobs -- --ignored --test-threads=1
//! ```
//!
//! Everything a test creates is named `dbine_test_jobs_*` and dropped at
//! the end, also when an assertion fails (the checks run under
//! `catch_unwind`, the cleanup after them on a fresh connection).

use dbine_driver::jobs::{JobList, JobOutcome, ServerJob};
use dbine_driver::{ConnectionConfig, Error, QueryOutcome, Session};
use futures::FutureExt;
use serde_json::Value;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

const DB: &str = "dbine_test_jobs_db";

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn connect(driver: &str, cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    assert!(d.capabilities().jobs, "{driver} lists jobs");
    d.connect(cfg, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.map(|_| out)
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The first column of the last result's rows, as text.
async fn column(s: &mut Box<dyn Session>, sql: &str) -> Vec<String> {
    let o = run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    o.results
        .last()
        .map(|r| {
            r.rows
                .iter()
                .map(|row| match &row[0] {
                    Value::String(v) => v.clone(),
                    Value::Null => String::new(),
                    v => v.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    column(s, sql).await.into_iter().next().unwrap_or_default()
}

fn find<'a>(list: &'a JobList, name: &str) -> &'a ServerJob {
    list.jobs
        .iter()
        .find(|j| j.name == name)
        .unwrap_or_else(|| panic!("{name} is listed: {list:#?}"))
}

/// Polls `jobs()` until `check` holds for the job named `name`.
async fn wait_for(
    s: &mut Box<dyn Session>,
    name: &str,
    secs: u64,
    check: impl Fn(&ServerJob) -> bool,
) -> Option<ServerJob> {
    let until = Instant::now() + Duration::from_secs(secs);
    loop {
        let list = s.jobs().await.unwrap();
        if let Some(j) = list.jobs.iter().find(|j| j.name == name && check(j)) {
            return Some(j.clone());
        }
        if Instant::now() > until {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Ids the driver didn't hand out are refused, before reaching the server.
async fn refuses_foreign_ids(s: &mut Box<dyn Session>, sample: &str) {
    let evil = [
        format!("{sample}; DROP DATABASE {DB}"),
        "`x`.`y`; DROP DATABASE dbine_test_jobs_db".to_string(),
        "x' OR '1'='1".to_string(),
        "job:`a` OR 1=1".to_string(),
        String::new(),
    ];
    for id in &evil {
        assert!(s.job_detail(id).await.is_err(), "{id}");
        assert!(s.set_job_enabled(id, false).await.is_err(), "{id}");
        assert!(s.job_script(id).await.is_err(), "{id}");
        assert!(s.stop_job(id).await.is_err(), "{id}");
        assert!(s.run_job(id).await.is_err(), "{id}");
        assert!(s.job_runs(id, 10).await.is_err(), "{id}");
    }
    let dbs = column(s, "SHOW DATABASES").await;
    assert!(dbs.iter().any(|d| d == DB), "{DB} still exists");
}

/// Runs `checks`, then `cleanup` on a fresh connection whatever happened,
/// then fails if the checks did.
async fn guarded<F, C>(driver: &str, cfg: &ConnectionConfig, checks: F, cleanup: C)
where
    F: std::future::Future<Output = ()>,
    C: AsyncFnOnce(&mut Box<dyn Session>),
{
    let outcome = AssertUnwindSafe(checks).catch_unwind().await;
    let mut s = connect(driver, cfg).await;
    cleanup(&mut s).await;
    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}

// ---- MySQL, MariaDB and OceanBase: events ----

async fn drop_events(s: &mut Box<dyn Session>) {
    // OceanBase keeps a dropped database's events: each one dropped first,
    // from its database.
    if run(s, &format!("USE {DB}")).await.is_ok() {
        for name in ["daily", "zoned", "once", "sleep"] {
            let _ = run(s, &format!("DROP EVENT IF EXISTS dbine_test_jobs_{name}")).await;
        }
        let _ = run(s, "USE information_schema").await;
    }
    let _ = run(s, &format!("DROP DATABASE IF EXISTS {DB}")).await;
    let _ = run(s, "DROP USER IF EXISTS 'dbine_test_jobs_u'@'%'").await;
    let _ = run(s, "DROP USER IF EXISTS 'dbine_test_jobs_r'@'%'").await;
    let _ = run(s, "DROP ROLE IF EXISTS dbine_test_jobs_role").await;
}

/// The next `hh:00:00` UTC after the server's now, as RFC 3339.
async fn next_daily(s: &mut Box<dyn Session>, hour: u32) -> String {
    scalar(
        s,
        &format!(
            "SELECT DATE_FORMAT(IF(TIME(UTC_TIMESTAMP()) < '{hour:02}:00:00', DATE(UTC_TIMESTAMP()), DATE(UTC_TIMESTAMP()) + INTERVAL 1 DAY), '%Y-%m-%dT{hour:02}:00:00Z')"
        ),
    )
    .await
}

/// `cfg` logging in as another user.
fn as_user(cfg: &ConnectionConfig, user: &str) -> ConnectionConfig {
    // OceanBase names the tenant after the user (`root@test`).
    let tenant = cfg
        .username
        .as_deref()
        .and_then(|u| u.split_once('@'))
        .map(|(_, t)| format!("@{t}"))
        .unwrap_or_default();
    ConnectionConfig {
        username: Some(format!("{user}{tenant}")),
        password: Some("dbine-Test-9".into()),
        ..cfg.clone()
    }
}

/// The running workers of an event, from performance_schema.
async fn workers(s: &mut Box<dyn Session>, name: &str) -> usize {
    column(
        s,
        &format!("SELECT t.PROCESSLIST_ID FROM performance_schema.events_statements_current e JOIN performance_schema.threads t ON t.THREAD_ID = e.THREAD_ID WHERE e.OBJECT_TYPE = 'EVENT' AND e.OBJECT_NAME = '{name}'"),
    )
    .await
    .len()
}

async fn events(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let ob = driver == "oceanbase";
    // The session's own zone, where times read as written (OceanBase has no
    // SYSTEM zone to set).
    let base_tz = if ob { "+00:00" } else { "SYSTEM" };
    let mut s = connect(driver, &cfg).await;
    drop_events(&mut s).await;
    let scheduler = scalar(&mut s, "SELECT @@global.event_scheduler").await;
    let scheduler_on = scheduler.eq_ignore_ascii_case("ON") || scheduler == "1";
    // OceanBase reads CREATE EVENT times on the tenant's global zone (an
    // offset, `+08:00` by default), whatever the session's.
    let zone = if ob {
        scalar(&mut s, "SELECT @@global.time_zone").await
    } else {
        "+00:00".to_string()
    };
    let offset_h: i32 = zone
        .trim_start_matches('+')
        .split(':')
        .next()
        .and_then(|h| h.parse().ok())
        .unwrap_or(0);
    let daily_utc = (3 - offset_h).rem_euclid(24) as u32;
    let checks = async {
        exec(&mut s, &format!("CREATE DATABASE {DB}")).await;
        // OceanBase creates events only in the current database.
        exec(&mut s, &format!("USE {DB}")).await;
        exec(
            &mut s,
            "CREATE USER 'dbine_test_jobs_u'@'%' IDENTIFIED BY 'dbine-Test-9'",
        )
        .await;
        exec(
            &mut s,
            &format!("GRANT EVENT ON {DB}.* TO 'dbine_test_jobs_u'@'%'"),
        )
        .await;
        exec(&mut s, "SET time_zone = '+00:00'").await;
        exec(
            &mut s,
            &format!(
                "CREATE DEFINER = 'dbine_test_jobs_u'@'%' EVENT {DB}.dbine_test_jobs_daily
                   ON SCHEDULE EVERY 1 DAY STARTS '2026-01-01 03:00:00'
                   COMMENT 'purga diaria' DO DELETE FROM {DB}.t WHERE 1 = 0"
            ),
        )
        .await;
        // A named zone without daylight saving: 03:00 there is 06:00 UTC
        // (OceanBase takes the global zone instead).
        if !ob {
            exec(&mut s, "SET time_zone = 'America/Argentina/Buenos_Aires'").await;
            exec(
                &mut s,
                &format!(
                    "CREATE EVENT {DB}.dbine_test_jobs_zoned ON SCHEDULE EVERY 1 DAY STARTS '2026-01-01 03:00:00' DO SELECT 1"
                ),
            )
            .await;
        }
        exec(&mut s, &format!("SET time_zone = '{base_tz}'")).await;
        exec(
            &mut s,
            &format!(
                "DELIMITER $$\nCREATE EVENT {DB}.dbine_test_jobs_once ON SCHEDULE AT '2030-01-01 00:00:00' ON COMPLETION PRESERVE DISABLE DO BEGIN INSERT INTO {DB}.t VALUES (2); INSERT INTO {DB}.t VALUES (3); END$$\nDELIMITER ;"
            ),
        )
        .await;
        exec(&mut s, &format!("CREATE TABLE {DB}.t (id INT)")).await;

        // While the scheduler is off: a note, no next runs.
        if !scheduler_on {
            let list = s.jobs().await.unwrap();
            eprintln!("{driver} (scheduler {scheduler}): {:?}", list.notes);
            assert!(
                list.notes.iter().any(|n| n.contains("event_scheduler")),
                "{:?}",
                list.notes
            );
            assert!(find(&list, "dbine_test_jobs_daily").next_run.is_none());
            // Turned on for the rest; turned back off at the end.
            exec(&mut s, "SET GLOBAL event_scheduler = ON").await;
        }

        // List.
        let list = s.jobs().await.unwrap();
        eprintln!("{driver}: {list:#?}");
        assert!(list.notes.is_empty(), "{:?}", list.notes);
        let daily = find(&list, "dbine_test_jobs_daily");
        assert_eq!(daily.kind, "Event");
        assert_eq!(daily.group.as_deref(), Some(DB));
        assert_eq!(daily.owner.as_deref(), Some("dbine_test_jobs_u@%"));
        assert_eq!(daily.description.as_deref(), Some("purga diaria"));
        assert_eq!(
            daily.schedule.as_deref(),
            Some("EVERY 1 DAY STARTS '2026-01-01 03:00:00'")
        );
        assert!(daily.enabled && daily.can_enable && daily.has_script);
        assert!(!daily.can_run && !daily.running);
        // Only OceanBase's scheduler logs the runs.
        assert_eq!(daily.has_history, ob);
        assert_eq!(
            daily.next_run.as_deref(),
            Some(next_daily(&mut s, daily_utc).await.as_str())
        );
        if !ob {
            let zoned = find(&list, "dbine_test_jobs_zoned");
            assert_eq!(
                zoned.next_run.as_deref(),
                Some(next_daily(&mut s, 6).await.as_str())
            );
        }
        let once = find(&list, "dbine_test_jobs_once");
        assert!(!once.enabled && once.next_run.is_none());
        assert_eq!(once.schedule.as_deref(), Some("AT '2030-01-01 00:00:00'"));

        // Detail.
        let d = s.job_detail(&daily.id).await.unwrap();
        assert_eq!(d.job.id, daily.id);
        assert_eq!(d.steps.len(), 1);
        assert_eq!(
            d.steps[0]
                .command
                .as_deref()
                .map(|c| c.trim_end_matches(';')),
            Some(format!("DELETE FROM {DB}.t WHERE 1 = 0").as_str())
        );
        let tz = d.properties.iter().find(|p| p.label == "Zona horaria");
        assert_eq!(tz.map(|p| p.value.as_str()), Some(zone.as_str()));

        // No run now; history only on OceanBase.
        assert!(matches!(
            s.run_job(&daily.id).await,
            Err(Error::Unsupported(_))
        ));
        if ob {
            assert!(s.job_runs(&daily.id, 0).await.unwrap().is_empty());
            s.job_runs(&daily.id, 10).await.unwrap();
        } else {
            assert!(matches!(
                s.job_runs(&daily.id, 10).await,
                Err(Error::Unsupported(_))
            ));
        }

        // Disable and enable, keeping the definer, from another database.
        exec(&mut s, "USE information_schema").await;
        s.set_job_enabled(&daily.id, false).await.unwrap();
        let j = wait_for(&mut s, "dbine_test_jobs_daily", 5, |j| !j.enabled)
            .await
            .expect("disabled");
        assert_eq!(j.owner.as_deref(), Some("dbine_test_jobs_u@%"));
        assert!(j.next_run.is_none());
        s.set_job_enabled(&daily.id, true).await.unwrap();
        let j = wait_for(&mut s, "dbine_test_jobs_daily", 5, |j| j.enabled)
            .await
            .expect("enabled");
        assert_eq!(j.owner.as_deref(), Some("dbine_test_jobs_u@%"));

        // Script: dropped and recreated from it, the same event.
        let row = format!("SELECT CONCAT_WS('|', EVENT_DEFINITION, EVENT_TYPE, EXECUTE_AT, STATUS, ON_COMPLETION, TIME_ZONE, DEFINER) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = '{DB}' AND EVENT_NAME = 'dbine_test_jobs_once'");
        let before = column(&mut s, &row).await;
        let script = s.job_script(&once.id).await.unwrap();
        eprintln!("{driver} script:\n{script}");
        assert!(
            script.contains(&format!("EVENT `{DB}`.`dbine_test_jobs_once`")),
            "{script}"
        );
        assert!(script.contains("DELIMITER"), "{script}");
        exec(&mut s, &format!("USE {DB}")).await;
        exec(&mut s, &format!("DROP EVENT {DB}.dbine_test_jobs_once")).await;
        // Run from another database.
        exec(&mut s, "USE information_schema").await;
        exec(&mut s, &script).await;
        let after = column(&mut s, &row).await;
        assert_eq!(before, after);
        assert_eq!(scalar(&mut s, "SELECT @@session.time_zone").await, base_tz);

        // Runs going on: seen and all stopped (MySQL's performance_schema,
        // OceanBase's scheduler). A run a second piles workers up on MySQL;
        // OceanBase ends an event's statement after 10 s.
        let ps = scalar(&mut s, "SELECT @@performance_schema").await == "1";
        if ps || ob {
            let secs = if ob { 9 } else { 60 };
            exec(&mut s, &format!("USE {DB}")).await;
            exec(
                &mut s,
                &format!(
                    "CREATE EVENT {DB}.dbine_test_jobs_sleep ON SCHEDULE EVERY 1 SECOND ON COMPLETION PRESERVE DO SELECT SLEEP({secs})"
                ),
            )
            .await;
            let j = wait_for(&mut s, "dbine_test_jobs_sleep", 25, |j| j.running)
                .await
                .expect("the sleeping event shows running");
            assert!(j.can_stop, "{j:?}");
            if ps {
                let until = Instant::now() + Duration::from_secs(10);
                while workers(&mut s, "dbine_test_jobs_sleep").await < 2 && Instant::now() < until {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                assert!(workers(&mut s, "dbine_test_jobs_sleep").await >= 2);
            }
            // No new runs, then every one going on stopped.
            s.set_job_enabled(&j.id, false).await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
            s.stop_job(&j.id).await.unwrap();
            let stopped = wait_for(&mut s, "dbine_test_jobs_sleep", 10, |j| !j.running).await;
            assert!(stopped.is_some(), "stopped");
            if ps {
                assert_eq!(workers(&mut s, "dbine_test_jobs_sleep").await, 0);
            }
            assert!(matches!(s.stop_job(&j.id).await, Err(Error::State(_))));
            if ob {
                // The scheduler logged how the run ended.
                let until = Instant::now() + Duration::from_secs(15);
                let runs = loop {
                    let runs = s.job_runs(&j.id, 10).await.unwrap();
                    if !runs.is_empty() || Instant::now() > until {
                        break runs;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                };
                eprintln!("{driver} sleep runs: {runs:#?}");
                assert!(!runs.is_empty());
                assert!(runs
                    .iter()
                    .all(|r| r.trigger.as_deref() == Some("programada")));
                let listed = find(&s.jobs().await.unwrap(), "dbine_test_jobs_sleep").clone();
                assert!(listed.last_outcome.is_some(), "{listed:?}");
            }
        } else {
            eprintln!("{driver}: performance_schema off; running events aren't visible");
        }

        // Who sees what: a user with EVENT on one database only is told the
        // list is partial; one with it on all through a role is not.
        let mut u = connect(driver, &as_user(&cfg, "dbine_test_jobs_u")).await;
        let list = u.jobs().await.unwrap();
        assert!(
            list.notes.iter().any(|n| n.contains("privilegio EVENT")),
            "{:?}",
            list.notes
        );
        assert!(list.jobs.iter().any(|j| j.name == "dbine_test_jobs_daily"));
        if !ob {
            exec(&mut s, "CREATE ROLE dbine_test_jobs_role").await;
            exec(&mut s, "GRANT EVENT ON *.* TO dbine_test_jobs_role").await;
            exec(
                &mut s,
                "CREATE USER 'dbine_test_jobs_r'@'%' IDENTIFIED BY 'dbine-Test-9'",
            )
            .await;
            exec(
                &mut s,
                "GRANT dbine_test_jobs_role TO 'dbine_test_jobs_r'@'%'",
            )
            .await;
            let default_role = if driver == "mariadb" {
                "SET DEFAULT ROLE dbine_test_jobs_role FOR 'dbine_test_jobs_r'@'%'"
            } else {
                "SET DEFAULT ROLE ALL TO 'dbine_test_jobs_r'@'%'"
            };
            exec(&mut s, default_role).await;
            let mut r = connect(driver, &as_user(&cfg, "dbine_test_jobs_r")).await;
            let list = r.jobs().await.unwrap();
            assert!(
                !list.notes.iter().any(|n| n.contains("privilegio EVENT")),
                "{:?}",
                list.notes
            );
            assert!(list.jobs.iter().any(|j| j.name == "dbine_test_jobs_daily"));
        }

        refuses_foreign_ids(&mut s, &daily.id).await;
    };
    guarded(driver, &cfg, checks, async |s| {
        drop_events(s).await;
        if !scheduler_on {
            let _ = run(s, &format!("SET GLOBAL event_scheduler = {scheduler}")).await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql() {
    events("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb() {
    events("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

/// `DBINE_TEST_OCEANBASE_URL=mysql://root@test:pw@localhost:25035`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn oceanbase() {
    events("oceanbase", "DBINE_TEST_OCEANBASE_URL").await;
}

// ---- StarRocks: tasks and materialized views ----

const DB2: &str = "dbine_test_jobs_db2";

async fn drop_starrocks(s: &mut Box<dyn Session>) {
    let _ = run(s, "DROP TASK IF EXISTS dbine_test_jobs_task").await;
    let _ = run(s, "DROP TASK IF EXISTS dbine_test_jobs_task2").await;
    let _ = run(s, "DROP USER IF EXISTS 'dbine_test_jobs_u'@'%'").await;
    let _ = run(s, &format!("DROP DATABASE IF EXISTS {DB2} FORCE")).await;
    let _ = run(
        s,
        &format!("DROP MATERIALIZED VIEW IF EXISTS {DB}.dbine_test_jobs_mv"),
    )
    .await;
    let _ = run(s, &format!("DROP DATABASE IF EXISTS {DB} FORCE")).await;
}

/// `DBINE_TEST_STARROCKS_URL=mysql://root@localhost:25030`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn starrocks() {
    let Some(cfg) = cfg("starrocks", "DBINE_TEST_STARROCKS_URL") else {
        eprintln!("DBINE_TEST_STARROCKS_URL not set; skipping");
        return;
    };
    let mut s = connect("starrocks", &cfg).await;
    drop_starrocks(&mut s).await;
    let checks = async {
        exec(&mut s, &format!("CREATE DATABASE {DB}")).await;
        exec(
            &mut s,
            &format!("CREATE TABLE {DB}.t (id INT, ts DATETIME) DUPLICATE KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 1 PROPERTIES('replication_num'='1')"),
        )
        .await;
        exec(
            &mut s,
            &format!("SUBMIT TASK `{DB}`.dbine_test_jobs_task SCHEDULE EVERY(INTERVAL 1 MINUTE) PROPERTIES('session.query_timeout'='120') AS INSERT INTO {DB}.t SELECT 1, now()"),
        )
        .await;
        exec(
            &mut s,
            &format!("CREATE MATERIALIZED VIEW {DB}.dbine_test_jobs_mv DISTRIBUTED BY HASH(id) REFRESH ASYNC EVERY(INTERVAL 1 HOUR) PROPERTIES('replication_num'='1') AS SELECT id, count(*) AS c FROM {DB}.t GROUP BY id"),
        )
        .await;

        // List.
        let list = s.jobs().await.unwrap();
        eprintln!("starrocks: {list:#?}");
        assert!(
            list.notes.iter().any(|n| n.contains("suspendida")),
            "{:?}",
            list.notes
        );
        // Root sees every database.
        assert!(
            !list.notes.iter().any(|n| n.contains("tiene privilegios")),
            "{:?}",
            list.notes
        );
        let task = find(&list, "dbine_test_jobs_task");
        assert_eq!(task.kind, "Task");
        assert_eq!(task.group.as_deref(), Some(DB));
        assert!(task.enabled && task.has_history && task.has_script);
        assert!(!task.can_run && !task.can_enable);
        assert!(
            task.schedule
                .as_deref()
                .unwrap()
                .contains("EVERY(1 MINUTES)"),
            "{task:?}"
        );
        let mv = find(&list, "dbine_test_jobs_mv");
        assert_eq!(mv.kind, "Materialized view");
        assert!(mv.id.starts_with("mv:`mv-"), "{mv:?}");
        assert!(mv.enabled && mv.can_run && mv.has_history && mv.has_script);

        // Detail.
        let d = s.job_detail(&task.id).await.unwrap();
        assert_eq!(
            d.steps[0].command.as_deref(),
            Some(format!("INSERT INTO {DB}.t SELECT 1, now()").as_str())
        );
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "session.query_timeout" && p.value == "120"));
        let d = s.job_detail(&mv.id).await.unwrap();
        assert!(d.properties.iter().any(|p| p.label == "Tarea"), "{d:?}");

        // History: the task ran when it was submitted.
        let until = Instant::now() + Duration::from_secs(30);
        let runs = loop {
            let runs = s.job_runs(&task.id, 10).await.unwrap();
            if runs.iter().any(|r| r.outcome == JobOutcome::Succeeded) || Instant::now() > until {
                break runs;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        eprintln!("starrocks task runs: {runs:#?}");
        assert!(
            runs.iter().any(|r| r.outcome == JobOutcome::Succeeded),
            "{runs:?}"
        );
        assert!(runs
            .iter()
            .all(|r| r.started.as_deref().is_some_and(|t| t.ends_with('Z'))));
        let j = wait_for(&mut s, "dbine_test_jobs_task", 5, |j| {
            j.last_outcome.is_some()
        })
        .await
        .unwrap();
        assert!(j.last_started.is_some());

        // Run now: a manual refresh of the view shows up in its history.
        let before = s.job_runs(&mv.id, 50).await.unwrap().len();
        s.run_job(&mv.id).await.unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        let runs = loop {
            let runs = s.job_runs(&mv.id, 50).await.unwrap();
            if runs.len() > before || Instant::now() > until {
                break runs;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        eprintln!("starrocks mv runs: {runs:#?}");
        assert!(runs.len() > before, "{runs:?}");
        assert!(
            runs.iter().any(|r| r.trigger.as_deref() == Some("manual")),
            "{runs:?}"
        );
        // Stopping a refresh that already ended does nothing.
        s.stop_job(&mv.id).await.unwrap();
        // A plain task can't be run or stopped.
        assert!(matches!(
            s.run_job(&task.id).await,
            Err(Error::Unsupported(_))
        ));

        // Enabling isn't offered: StarRocks doesn't report suspension.
        assert!(matches!(
            s.set_job_enabled(&task.id, false).await,
            Err(Error::Unsupported(_))
        ));

        // Scripts: the task recreated from its own is the same task.
        let script = s.job_script(&task.id).await.unwrap();
        eprintln!("starrocks task script:\n{script}");
        assert!(
            script.starts_with(&format!("SUBMIT TASK `{DB}`.`dbine_test_jobs_task`")),
            "{script}"
        );
        exec(&mut s, "DROP TASK dbine_test_jobs_task").await;
        exec(&mut s, &script).await;
        let back = column(
            &mut s,
            "SELECT CONCAT(DEFINITION, '|', PROPERTIES) FROM information_schema.tasks WHERE TASK_NAME = 'dbine_test_jobs_task'",
        )
        .await;
        assert_eq!(back.len(), 1, "{back:?}");
        assert!(
            back[0].starts_with(&format!("INSERT INTO {DB}.t SELECT 1, now()|")),
            "{back:?}"
        );
        assert!(
            back[0].contains("'session.query_timeout'='120'"),
            "{back:?}"
        );
        let script = s.job_script(&mv.id).await.unwrap();
        assert!(
            script.contains(&format!("MATERIALIZED VIEW `{DB}`.`dbine_test_jobs_mv`")),
            "{script}"
        );

        // A user with privileges on one database: told the list is partial,
        // and another database's task isn't in it.
        exec(&mut s, &format!("CREATE DATABASE {DB2}")).await;
        exec(
            &mut s,
            &format!("CREATE TABLE {DB2}.t (id INT) DUPLICATE KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 1 PROPERTIES('replication_num'='1')"),
        )
        .await;
        exec(
            &mut s,
            &format!("SUBMIT TASK `{DB2}`.dbine_test_jobs_task2 SCHEDULE EVERY(INTERVAL 1 HOUR) AS INSERT INTO {DB2}.t SELECT 1"),
        )
        .await;
        exec(
            &mut s,
            "CREATE USER 'dbine_test_jobs_u'@'%' IDENTIFIED BY 'dbine-Test-9'",
        )
        .await;
        exec(
            &mut s,
            &format!("GRANT ALL ON DATABASE {DB} TO USER 'dbine_test_jobs_u'@'%'"),
        )
        .await;
        let mut u = connect("starrocks", &as_user(&cfg, "dbine_test_jobs_u")).await;
        let list = u.jobs().await.unwrap();
        eprintln!("starrocks as dbine_test_jobs_u: {list:#?}");
        assert!(
            list.notes.iter().any(|n| n.contains("tiene privilegios")),
            "{:?}",
            list.notes
        );
        assert!(list.jobs.iter().any(|j| j.name == "dbine_test_jobs_task"));
        assert!(!list.jobs.iter().any(|j| j.name == "dbine_test_jobs_task2"));
        let all = s.jobs().await.unwrap();
        assert!(all.jobs.iter().any(|j| j.name == "dbine_test_jobs_task2"));

        // At most the runs asked for: none for 0.
        assert!(s.job_runs(&task.id, 0).await.unwrap().is_empty());

        refuses_foreign_ids(&mut s, &task.id).await;
    };
    guarded("starrocks", &cfg, checks, async |s| drop_starrocks(s).await).await;
}

// ---- Doris: jobs and materialized views ----

async fn drop_doris(s: &mut Box<dyn Session>) {
    let _ = run(s, "DROP JOB WHERE jobName = 'dbine_test_jobs_job'").await;
    let _ = run(s, "DROP JOB WHERE jobName = 'dbine_test_jobs_bare'").await;
    let _ = run(s, "DROP USER IF EXISTS 'dbine_test_jobs_u'@'%'").await;
    let _ = run(
        s,
        &format!("DROP MATERIALIZED VIEW IF EXISTS {DB}.dbine_test_jobs_mv"),
    )
    .await;
    let _ = run(s, &format!("DROP DATABASE IF EXISTS {DB} FORCE")).await;
}

/// `DBINE_TEST_DORIS_URL=mysql://root@localhost:25031`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn doris() {
    let Some(cfg) = cfg("doris", "DBINE_TEST_DORIS_URL") else {
        eprintln!("DBINE_TEST_DORIS_URL not set; skipping");
        return;
    };
    let mut s = connect("doris", &cfg).await;
    drop_doris(&mut s).await;
    let checks = async {
        exec(&mut s, &format!("CREATE DATABASE {DB}")).await;
        exec(
            &mut s,
            &format!("CREATE TABLE {DB}.t (id INT, ts DATETIME) DUPLICATE KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 1 PROPERTIES('replication_num'='1')"),
        )
        .await;
        exec(&mut s, &format!("USE {DB}")).await;
        let starts = scalar(
            &mut s,
            "SELECT DATE_FORMAT(NOW() + INTERVAL 3 SECOND, '%Y-%m-%d %H:%i:%s')",
        )
        .await;
        exec(
            &mut s,
            &format!("CREATE JOB dbine_test_jobs_job ON SCHEDULE EVERY 1 MINUTE STARTS '{starts}' COMMENT 'cada minuto' DO INSERT INTO {DB}.t SELECT 1, now()"),
        )
        .await;
        exec(
            &mut s,
            &format!("CREATE MATERIALIZED VIEW {DB}.dbine_test_jobs_mv BUILD IMMEDIATE REFRESH AUTO ON SCHEDULE EVERY 1 HOUR DISTRIBUTED BY RANDOM BUCKETS 1 PROPERTIES('replication_num'='1') AS SELECT id, count(*) AS c FROM {DB}.t GROUP BY id"),
        )
        .await;

        // List.
        let list = s.jobs().await.unwrap();
        eprintln!("doris: {list:#?}");
        assert!(list.notes.is_empty(), "{:?}", list.notes);
        let job = find(&list, "dbine_test_jobs_job");
        assert_eq!(job.kind, "Job");
        assert_eq!(job.description.as_deref(), Some("cada minuto"));
        assert_eq!(
            job.schedule.as_deref(),
            Some(format!("EVERY 1 MINUTE STARTS {starts}").as_str())
        );
        assert!(job.enabled && job.can_enable && job.has_history && job.has_script);
        assert!(!job.can_run);
        assert!(
            job.next_run.as_deref().is_some_and(|t| t.ends_with('Z')),
            "{job:?}"
        );
        let mv = find(&list, "dbine_test_jobs_mv");
        assert_eq!(mv.kind, "Materialized view");
        assert_eq!(mv.group.as_deref(), Some(DB));
        assert!(mv.enabled && mv.can_run && mv.can_enable && mv.has_script);
        assert!(mv.next_run.is_some(), "{mv:?}");

        // Detail.
        let d = s.job_detail(&job.id).await.unwrap();
        assert_eq!(
            d.steps[0].command.as_deref(),
            Some(format!("INSERT INTO {DB}.t SELECT 1, now()").as_str())
        );

        // History: the first scheduled run.
        let until = Instant::now() + Duration::from_secs(45);
        let runs = loop {
            let runs = s.job_runs(&job.id, 10).await.unwrap();
            if runs.iter().any(|r| r.outcome == JobOutcome::Succeeded) || Instant::now() > until {
                break runs;
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        };
        eprintln!("doris job runs: {runs:#?}");
        assert!(
            runs.iter().any(|r| r.outcome == JobOutcome::Succeeded),
            "{runs:?}"
        );
        let j = wait_for(&mut s, "dbine_test_jobs_job", 5, |j| {
            j.last_outcome.is_some()
        })
        .await
        .unwrap();
        assert!(
            j.last_started.is_some() && j.last_duration_ms.is_some(),
            "{j:?}"
        );

        // Run now: the view's manual refresh.
        assert!(matches!(
            s.run_job(&job.id).await,
            Err(Error::Unsupported(_))
        ));
        s.run_job(&mv.id).await.unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        let runs = loop {
            let runs = s.job_runs(&mv.id, 50).await.unwrap();
            if runs.iter().any(|r| r.trigger.as_deref() == Some("manual")) || Instant::now() > until
            {
                break runs;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        eprintln!("doris mv runs: {runs:#?}");
        assert!(
            runs.iter().any(|r| r.trigger.as_deref() == Some("manual")),
            "{runs:?}"
        );
        // Stop: the refresh may be over already.
        match s.stop_job(&mv.id).await {
            Ok(()) | Err(Error::State(_)) => {}
            Err(e) => panic!("stop: {e}"),
        }

        // Disable and enable, both kinds.
        for (name, id) in [
            ("dbine_test_jobs_job", &job.id),
            ("dbine_test_jobs_mv", &mv.id),
        ] {
            s.set_job_enabled(id, false).await.unwrap();
            let j = wait_for(&mut s, name, 10, |j| !j.enabled)
                .await
                .expect("paused");
            assert!(j.next_run.is_none() && j.can_enable);
            s.set_job_enabled(id, true).await.unwrap();
            wait_for(&mut s, name, 10, |j| j.enabled)
                .await
                .expect("resumed");
        }

        // Scripts: the job recreated from its own is the same job.
        let script = s.job_script(&job.id).await.unwrap();
        eprintln!("doris job script:\n{script}");
        assert!(script.contains(&format!("STARTS '{starts}'")), "{script}");
        exec(&mut s, "DROP JOB WHERE jobName = 'dbine_test_jobs_job'").await;
        exec(&mut s, &script).await;
        let back = column(
            &mut s,
            "SELECT CONCAT(ExecuteSql, '|', RecurringStrategy, '|', Comment) FROM jobs('type'='insert') WHERE Name = 'dbine_test_jobs_job'",
        )
        .await;
        assert_eq!(
            back,
            vec![format!(
                "INSERT INTO {DB}.t SELECT 1, now()|EVERY 1 MINUTE STARTS {starts}|cada minuto"
            )]
        );
        let script = s.job_script(&mv.id).await.unwrap();
        assert!(
            script.contains(&format!("MATERIALIZED VIEW `{DB}`.`dbine_test_jobs_mv`")),
            "{script}"
        );
        // A job naming its table bare: the script says to run it in its
        // database, and does the same there.
        exec(
            &mut s,
            "CREATE JOB dbine_test_jobs_bare ON SCHEDULE EVERY 1 HOUR STARTS '2030-01-01 00:00:00' DO INSERT INTO t SELECT 2, now()",
        )
        .await;
        let bare = find(&s.jobs().await.unwrap(), "dbine_test_jobs_bare").clone();
        let script = s.job_script(&bare.id).await.unwrap();
        eprintln!("doris bare script:\n{script}");
        assert!(script.starts_with("-- Doris no informa"), "{script}");
        exec(&mut s, "DROP JOB WHERE jobName = 'dbine_test_jobs_bare'").await;
        exec(&mut s, &script).await;
        let back = column(
            &mut s,
            "SELECT ExecuteSql FROM jobs('type'='insert') WHERE Name = 'dbine_test_jobs_bare'",
        )
        .await;
        assert_eq!(back, vec!["INSERT INTO t SELECT 2, now()".to_string()]);

        // Without ADMIN: no insert jobs, the views' jobs it may see, and
        // notes saying both.
        exec(
            &mut s,
            "CREATE USER 'dbine_test_jobs_u'@'%' IDENTIFIED BY 'dbine-Test-9'",
        )
        .await;
        exec(
            &mut s,
            &format!("GRANT SELECT_PRIV ON {DB}.* TO 'dbine_test_jobs_u'@'%'"),
        )
        .await;
        let mut u = connect("doris", &as_user(&cfg, "dbine_test_jobs_u")).await;
        let list = u.jobs().await.unwrap();
        eprintln!("doris as dbine_test_jobs_u: {list:#?}");
        assert_eq!(list.notes.len(), 2, "{:?}", list.notes);
        assert!(list.notes[0].contains("ADMIN"), "{:?}", list.notes);
        assert!(
            list.notes[1].contains("vistas materializadas"),
            "{:?}",
            list.notes
        );
        assert!(!list.jobs.iter().any(|j| j.name == "dbine_test_jobs_job"));
        assert!(list.jobs.iter().any(|j| j.name == "dbine_test_jobs_mv"));

        // At most the runs asked for: none for 0.
        assert!(s.job_runs(&job.id, 0).await.unwrap().is_empty());

        refuses_foreign_ids(&mut s, &job.id).await;
    };
    guarded("doris", &cfg, checks, async |s| drop_doris(s).await).await;
}

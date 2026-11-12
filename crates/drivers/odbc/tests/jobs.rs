//! Server jobs against a real server through the engine's own ODBC driver
//! and the engine's preset: each test creates a job named
//! `dbine_test_jobs_…`, goes through list, detail, script, disable and
//! enable, run now and the history it leaves (where the engine has them),
//! checks that forged ids are refused, and removes what it created before
//! asserting anything.
//!
//! Then it runs the job's script after removing the job, to check that it
//! recreates it (with Db2, from a task whose begin time has passed).
//!
//! One variable per engine, with the connection fields as `key=value;…`
//! (`host`, `port`, `database`, `user`, `password`,
//! `odbc_driver`, `extra`):
//!
//! ```sh
//! # Informix (icr.io/informix/informix-developer-database, amd64), connected to sysadmin
//! DBINE_TEST_ODBC_JOBS_INFORMIX='host=127.0.0.1;port=9088;database=sysadmin;user=informix;password=in4mix;odbc_driver=IBM INFORMIX ODBC DRIVER;extra=SERVER=informix' \
//! # SQL Anywhere
//! DBINE_TEST_ODBC_JOBS_SQLANYWHERE='host=127.0.0.1;port=2638;database=demo;user=DBA;password=sql;odbc_driver=SQL Anywhere 17' \
//! # Hive 4 (apache/hive:4.0.0), through a Hive ODBC driver
//! DBINE_TEST_ODBC_JOBS_HIVE='host=127.0.0.1;port=10000;user=hive;odbc_driver=Cloudera ODBC Driver for Apache Hive' \
//! # Db2 LUW (icr.io/db2_community/db2, amd64), DB2_ATS_ENABLE=YES and SYSTOOLSPACE
//! DBINE_TEST_ODBC_JOBS_DB2='host=127.0.0.1;port=50000;database=testdb;user=db2inst1;password=…;odbc_driver=IBM DB2 ODBC DRIVER' \
//!   cargo test -p dbine-driver-odbc --test jobs -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, ServerJob, Session};
use std::time::{Duration, Instant};

const FORGED: &[&str] = &["1; DROP TABLE x", "' OR '1'='1", "1 OR 1=1", "../../x", ""];

/// The connection from `var`, or `None` (the test is skipped).
fn cfg(var: &str, preset: &str) -> Option<ConnectionConfig> {
    let raw = std::env::var(var).ok()?;
    let mut c = ConnectionConfig {
        driver: preset.into(),
        ..Default::default()
    };
    for part in raw.split(';') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k.trim() {
            "host" => c.host = v.into(),
            "port" => c.port = v.parse().unwrap_or(0),
            "database" => c.database = v.into(),
            "user" => c.username = Some(v.into()),
            "password" => c.password = Some(v.into()),
            // `extra=SERVER=x` keeps everything after the first `=`.
            key => {
                c.options.insert(key.into(), v.into());
            }
        }
    }
    Some(c)
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_odbc::drivers()
        .into_iter()
        .find(|d| d.info().id == c.driver)
        .expect("preset");
    assert!(d.capabilities().jobs, "{} has jobs", c.driver);
    d.connect(c, None).await.expect("connect")
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .map_err(|e| format!("{e}\n---\n{sql}"))
}

fn check(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what.into())
    }
}

async fn find(s: &mut Box<dyn Session>, name: &str) -> Result<ServerJob, String> {
    let list = s.jobs().await.map_err(|e| e.to_string())?;
    list.jobs
        .iter()
        .find(|j| j.name == name)
        .cloned()
        .ok_or_else(|| format!("{name} isn't listed: {list:?}"))
}

/// Everything the job allows, checked against the server.
async fn flow(s: &mut Box<dyn Session>, name: &str) -> Result<(), String> {
    let job = find(s, name).await?;
    println!("{job:?}");
    let d = s.job_detail(&job.id).await.map_err(|e| e.to_string())?;
    check(
        d.job.id == job.id && d.job.name == name,
        "detail of the job",
    )?;
    if job.has_script {
        let sc = s.job_script(&job.id).await.map_err(|e| e.to_string())?;
        println!("{sc}");
        check(sc.contains(name), format!("the script names the job: {sc}"))?;
    }
    if job.can_enable {
        s.set_job_enabled(&job.id, false)
            .await
            .map_err(|e| e.to_string())?;
        check(!find(s, name).await?.enabled, "disabled")?;
        s.set_job_enabled(&job.id, true)
            .await
            .map_err(|e| e.to_string())?;
        check(find(s, name).await?.enabled, "enabled again")?;
    } else {
        check(
            s.set_job_enabled(&job.id, false).await.is_err(),
            "no enable",
        )?;
    }
    if job.can_run {
        s.run_job(&job.id).await.map_err(|e| e.to_string())?;
        if job.has_history {
            let until = Instant::now() + Duration::from_secs(90);
            loop {
                let runs = s.job_runs(&job.id, 10).await.map_err(|e| e.to_string())?;
                if !runs.is_empty() {
                    println!("{runs:?}");
                    break;
                }
                check(Instant::now() < until, "a run in the history after run now")?;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    } else {
        check(s.run_job(&job.id).await.is_err(), "no run now")?;
    }
    for bad in FORGED {
        check(
            s.job_detail(bad).await.is_err(),
            format!("detail of {bad:?}"),
        )?;
        check(s.run_job(bad).await.is_err(), format!("run of {bad:?}"))?;
        check(
            s.set_job_enabled(bad, false).await.is_err(),
            format!("enable of {bad:?}"),
        )?;
        check(
            s.job_runs(bad, 5).await.is_err(),
            format!("runs of {bad:?}"),
        )?;
    }
    Ok(())
}

/// The job's script recreates it: `remove` drops the job, the script runs,
/// and the job is listed again. `expect` is text the script must have.
async fn replay(
    s: &mut Box<dyn Session>,
    name: &str,
    remove: &[&str],
    expect: &str,
) -> Result<(), String> {
    // So a begin time set a few seconds ahead in `setup` has passed.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let job = find(s, name).await?;
    let sc = s.job_script(&job.id).await.map_err(|e| e.to_string())?;
    println!("{sc}");
    check(
        sc.contains(expect),
        format!("the script has {expect:?}: {sc}"),
    )?;
    for sql in remove {
        exec(s, sql).await?;
    }
    check(find(s, name).await.is_err(), "removed before the replay")?;
    exec(s, &sc).await?;
    find(s, name).await.map(|_| ())
}

/// Runs `setup`, the flow and the replay of the job's script (when `remove`
/// isn't empty), then `cleanup` whatever happened, and only then fails.
async fn with_job(
    c: ConnectionConfig,
    name: &str,
    setup: &[&str],
    cleanup: &[&str],
    remove: &[&str],
    expect: &str,
) {
    let mut s = open(&c).await;
    for sql in cleanup {
        let _ = exec(&mut s, sql).await;
    }
    let mut r = Ok(());
    for sql in setup {
        r = exec(&mut s, sql).await;
        if r.is_err() {
            break;
        }
    }
    if r.is_ok() {
        r = flow(&mut s, name).await;
    }
    if r.is_ok() && !remove.is_empty() {
        r = replay(&mut s, name, remove, expect).await;
    }
    let mut left = Vec::new();
    for sql in cleanup {
        if let Err(e) = exec(&mut s, sql).await {
            left.push(e);
        }
    }
    r.unwrap();
    assert!(left.is_empty(), "cleanup failed: {left:?}");
}

const IFX_CLEANUP: &[&str] = &[
    "DELETE FROM ph_alert WHERE alert_task_id IN (SELECT tk_id FROM ph_task WHERE tk_name LIKE 'dbine_test_jobs_%')",
    "DELETE FROM ph_run WHERE run_task_id IN (SELECT tk_id FROM ph_task WHERE tk_name LIKE 'dbine_test_jobs_%')",
    "DELETE FROM ph_task WHERE tk_name LIKE 'dbine_test_jobs_%'",
];

#[tokio::test]
#[ignore]
async fn informix_scheduler_tasks() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_INFORMIX", "informix") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_INFORMIX not set");
        return;
    };
    with_job(
        c,
        "dbine_test_jobs_ifx",
        &["INSERT INTO ph_task (tk_name, tk_type, tk_group, tk_description, tk_execute, tk_start_time, tk_stop_time, tk_frequency, tk_enable)
           VALUES ('dbine_test_jobs_ifx', 'TASK', 'MISC', 'dbine test', 'SELECT COUNT(*) FROM systables',
                   DATETIME(03:00:00) HOUR TO SECOND, NULL, INTERVAL(1) DAY TO DAY, 't')"],
        IFX_CLEANUP,
        IFX_CLEANUP,
        "INSERT INTO ph_task",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn sql_anywhere_events() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_SQLANYWHERE", "sqlanywhere") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_SQLANYWHERE not set");
        return;
    };
    with_job(
        c,
        "dbine_test_jobs_sa",
        &["CREATE EVENT dbine_test_jobs_sa SCHEDULE START TIME '03:00' EVERY 24 HOURS HANDLER BEGIN DECLARE x INT; SET x = 1; END"],
        &["DROP EVENT IF EXISTS dbine_test_jobs_sa"],
        &["DROP EVENT dbine_test_jobs_sa"],
        "CREATE EVENT",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn db2_admin_task_scheduler() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_DB2", "db2") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_DB2 not set");
        return;
    };
    with_job(
        c,
        "dbine_test_jobs_ats",
        // Begins two seconds from now, so the script sees a begin in the
        // past: ADMIN_TASK_ADD refuses that, and the script leaves it out.
        &["CALL SYSPROC.ADMIN_TASK_ADD('dbine_test_jobs_ats', CURRENT TIMESTAMP + 2 SECONDS, NULL, NULL, '0 3 * * *', 'SYSPROC', 'ADMIN_CMD', 'SELECT ''RUNSTATS ON TABLE SYSIBM.SYSTABLES'' FROM SYSIBM.SYSDUMMY1', NULL, 'dbine test')"],
        &["CALL SYSPROC.ADMIN_TASK_REMOVE('dbine_test_jobs_ats', NULL)"],
        &["CALL SYSPROC.ADMIN_TASK_REMOVE('dbine_test_jobs_ats', NULL)"],
        "-- Inicio original:",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn hive_scheduled_queries() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_HIVE", "hive") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_HIVE not set");
        return;
    };
    with_job(
        c,
        "dbine_test_jobs_hive",
        &["CREATE SCHEDULED QUERY dbine_test_jobs_hive CRON '0 0 3 * * ? *' ENABLED DEFINED AS SELECT 1"],
        &["DROP SCHEDULED QUERY dbine_test_jobs_hive"],
        &["DROP SCHEDULED QUERY dbine_test_jobs_hive"],
        "CREATE SCHEDULED QUERY",
    )
    .await;
}

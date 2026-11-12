//! Server jobs against a real server through the engine's own ODBC driver
//! and the engine's preset: each test creates a job named
//! `dbine_test_jobs_…` (Dameng's DBMS_JOB one, which has no name of its
//! own, is number 990731), goes through list, detail, script, disable and
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
//! # Vertica 11.1.1 or later, as a superuser (dbadmin)
//! DBINE_TEST_ODBC_JOBS_VERTICA='host=127.0.0.1;port=5433;database=vmart;user=dbadmin;password=…;odbc_driver=Vertica' \
//! # Dameng (DM8) as SYSDBA, with the job system created (SP_INIT_JOB_SYS(1))
//! # and DBMS_SCHEDULER's (SP_INIT_DBMS_SCHEDULER_SYS(1))
//! DBINE_TEST_ODBC_JOBS_DAMENG='host=127.0.0.1;port=5236;user=SYSDBA;password=…;odbc_driver=DM8 ODBC DRIVER' \
//! # Altibase as SYS (CREATE JOB is SYS's), JOB_SCHEDULER_ENABLE=1
//! DBINE_TEST_ODBC_JOBS_ALTIBASE='host=127.0.0.1;port=20300;user=SYS;password=manager;odbc_driver=ALTIBASE_HDB_ODBC_64bit' \
//! # Virtuoso (openlink/virtuoso-opensource-7) as dba
//! DBINE_TEST_ODBC_JOBS_VIRTUOSO='host=127.0.0.1;port=1111;user=dba;password=dba;odbc_driver=OpenLink Virtuoso' \
//!   cargo test -p dbine-driver-odbc --test jobs -- --ignored --nocapture
//! ```
//!
//! Names are compared without case: Altibase and Dameng keep them in
//! capitals.

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

/// How a test finds its job: by name, or by id (a DBMS_JOB job, whose
/// name the server picks).
#[derive(Clone, Copy, Debug)]
enum Pick<'a> {
    Name(&'a str),
    Id(&'a str),
}

impl Pick<'_> {
    fn is(&self, j: &ServerJob) -> bool {
        match self {
            Pick::Name(n) => j.name.eq_ignore_ascii_case(n),
            Pick::Id(id) => j.id == *id,
        }
    }

    /// What the job's script must have, in lower case.
    fn mark(&self) -> String {
        match self {
            Pick::Name(n) => n.to_lowercase(),
            Pick::Id(id) => id.to_string(),
        }
    }
}

async fn find(s: &mut Box<dyn Session>, pick: Pick<'_>) -> Result<ServerJob, String> {
    let list = s.jobs().await.map_err(|e| e.to_string())?;
    list.jobs
        .iter()
        .find(|j| pick.is(j))
        .cloned()
        .ok_or_else(|| format!("{pick:?} isn't listed: {list:?}"))
}

/// Everything the job allows, checked against the server. `outside`
/// turns the job off without DBine (DBMS_JOB's BROKEN): enabling it must
/// bring it back.
async fn flow(
    s: &mut Box<dyn Session>,
    pick: Pick<'_>,
    outside: Option<&str>,
) -> Result<(), String> {
    let job = find(s, pick).await?;
    println!("{job:?}");
    let d = s.job_detail(&job.id).await.map_err(|e| e.to_string())?;
    check(d.job.id == job.id && pick.is(&d.job), "detail of the job")?;
    if job.has_script {
        let sc = s.job_script(&job.id).await.map_err(|e| e.to_string())?;
        println!("{sc}");
        check(
            sc.to_lowercase().contains(&pick.mark()),
            format!("the script names the job: {sc}"),
        )?;
    }
    if job.can_enable {
        s.set_job_enabled(&job.id, false)
            .await
            .map_err(|e| e.to_string())?;
        check(!find(s, pick).await?.enabled, "disabled")?;
        s.set_job_enabled(&job.id, true)
            .await
            .map_err(|e| e.to_string())?;
        check(find(s, pick).await?.enabled, "enabled again")?;
        if let Some(sql) = outside {
            exec(s, sql).await?;
            let off = find(s, pick).await?;
            check(
                !off.enabled && off.can_enable,
                format!("off outside DBine: {off:?}"),
            )?;
            s.set_job_enabled(&job.id, true)
                .await
                .map_err(|e| e.to_string())?;
            check(find(s, pick).await?.enabled, "enabled after being off")?;
        }
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
    pick: Pick<'_>,
    remove: &[&str],
    expect: &str,
) -> Result<(), String> {
    // So a begin time set a few seconds ahead in `setup` has passed.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let job = find(s, pick).await?;
    let sc = s.job_script(&job.id).await.map_err(|e| e.to_string())?;
    println!("{sc}");
    check(
        sc.contains(expect),
        format!("the script has {expect:?}: {sc}"),
    )?;
    for sql in remove {
        exec(s, sql).await?;
    }
    check(find(s, pick).await.is_err(), "removed before the replay")?;
    exec(s, &sc).await?;
    find(s, pick).await.map(|_| ())
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
    let job = Job {
        pick: Pick::Name(name),
        setup,
        cleanup,
        remove,
        expect,
        outside: None,
    };
    with(c, job).await;
}

/// A test's job: how it's found, made, removed and recreated.
struct Job<'a> {
    pick: Pick<'a>,
    setup: &'a [&'a str],
    /// Run before `setup` (errors ignored) and at the end (they fail).
    cleanup: &'a [&'a str],
    /// What drops the job before its script replays; empty: no replay.
    remove: &'a [&'a str],
    /// Text the script must have.
    expect: &'a str,
    /// Turns the job off without DBine (see [`flow`]).
    outside: Option<&'a str>,
}

/// [`with_job`] for any [`Job`].
async fn with(c: ConnectionConfig, job: Job<'_>) {
    let Job {
        pick,
        setup,
        cleanup,
        remove,
        expect,
        outside,
    } = job;
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
        r = flow(&mut s, pick, outside).await;
    }
    if r.is_ok() && !remove.is_empty() {
        r = replay(&mut s, pick, remove, expect).await;
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

#[tokio::test]
#[ignore]
async fn vertica_scheduled_triggers() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_VERTICA", "vertica") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_VERTICA not set");
        return;
    };
    with_job(
        c,
        "dbine_test_jobs_vt",
        &[
            "CREATE OR REPLACE PROCEDURE public.dbine_test_jobs_vproc() LANGUAGE PLvSQL AS $$ BEGIN RAISE NOTICE 'dbine'; END; $$",
            "CREATE SCHEDULE public.dbine_test_jobs_vsched USING CRON '0 3 * * *'",
            "CREATE TRIGGER public.dbine_test_jobs_vt ON SCHEDULE public.dbine_test_jobs_vsched EXECUTE PROCEDURE public.dbine_test_jobs_vproc() AS DEFINER",
        ],
        &[
            "DROP TRIGGER IF EXISTS public.dbine_test_jobs_vt",
            "DROP SCHEDULE IF EXISTS public.dbine_test_jobs_vsched",
            "DROP PROCEDURE IF EXISTS public.dbine_test_jobs_vproc()",
        ],
        &[
            "DROP TRIGGER public.dbine_test_jobs_vt",
            "DROP SCHEDULE public.dbine_test_jobs_vsched",
        ],
        "CREATE TRIGGER",
    )
    .await;
}

const DM_CLEANUP: &[&str] = &[
    "SP_JOB_CLEAR_HISTORIES('dbine_test_jobs_dm')",
    "SP_DROP_JOB('dbine_test_jobs_dm')",
];

#[tokio::test]
#[ignore]
async fn dameng_job_system() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_DAMENG", "dameng") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_DAMENG not set");
        return;
    };
    with_job(
        c,
        "dbine_test_jobs_dm",
        &[
            "SP_CREATE_JOB('dbine_test_jobs_dm', 1, 0, '', 0, 0, '', 0, 'dbine test')",
            "SP_JOB_CONFIG_START('dbine_test_jobs_dm')",
            "SP_ADD_JOB_STEP('dbine_test_jobs_dm', 'S1', 0, 'SELECT 1 FROM DUAL;', 1, 1, 0, 0, NULL, 0)",
            "SP_ADD_JOB_SCHEDULE('dbine_test_jobs_dm', 'D1', 1, 1, 1, 0, 0, '03:00:00', NULL, '2026-01-01', NULL, '')",
            "SP_JOB_CONFIG_COMMIT('dbine_test_jobs_dm')",
        ],
        DM_CLEANUP,
        DM_CLEANUP,
        "SP_CREATE_JOB",
    )
    .await;
}

/// DM keeps DBMS_SCHEDULER's names in capitals.
const DMS_DROP: &str = "BEGIN DBMS_SCHEDULER.DROP_JOB('DBINE_TEST_JOBS_DMS', TRUE); END;\n/";

#[tokio::test]
#[ignore]
async fn dameng_dbms_scheduler() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_DAMENG", "dameng") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_DAMENG not set");
        return;
    };
    with(
        c,
        Job {
            pick: Pick::Name("dbine_test_jobs_dms"),
            setup: &["BEGIN DBMS_SCHEDULER.CREATE_JOB(JOB_NAME => 'DBINE_TEST_JOBS_DMS', JOB_TYPE => 'PLSQL_BLOCK', JOB_ACTION => 'BEGIN NULL; END;', REPEAT_INTERVAL => 'FREQ=DAILY', ENABLED => TRUE, COMMENTS => 'dbine test'); END;\n/"],
            cleanup: &[
                DMS_DROP,
                // Its runs' log, where the server can purge it.
                "BEGIN DBMS_SCHEDULER.PURGE_LOG(0, 'JOB_LOG', 'DBINE_TEST_JOBS_DMS'); EXCEPTION WHEN OTHERS THEN NULL; END;\n/",
            ],
            remove: &[DMS_DROP],
            expect: "DBMS_SCHEDULER.CREATE_JOB",
            outside: None,
        },
    )
    .await;
}

/// A DBMS_JOB job by its number: its name in SYSJOBS is the server's, so
/// it's found by id (which is that number in SYSJOBS too).
const DMJ: &str = "990731";
const DMJ_REMOVE: &str = "BEGIN DBMS_JOB.REMOVE(990731); END;\n/";

#[tokio::test]
#[ignore]
async fn dameng_dbms_job() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_DAMENG", "dameng") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_DAMENG not set");
        return;
    };
    with(
        c,
        Job {
            pick: Pick::Id(DMJ),
            setup: &["BEGIN DBMS_JOB.ISUBMIT(990731, 'NULL;', SYSDATE + 1, 'SYSDATE + 1'); END;\n/"],
            cleanup: &[
                // Its runs, by the name the server gave it.
                "DECLARE n VARCHAR(128); BEGIN SELECT NAME INTO n FROM SYSJOB.SYSJOBS WHERE ID = 990731; SP_JOB_CLEAR_HISTORIES(n); EXCEPTION WHEN OTHERS THEN NULL; END;\n/",
                DMJ_REMOVE,
            ],
            remove: &[DMJ_REMOVE],
            expect: "DBMS_JOB.ISUBMIT",
            // Broken outside DBine: enabling takes it out of BROKEN.
            outside: Some("BEGIN DBMS_JOB.BROKEN(990731, TRUE); END;\n/"),
        },
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn altibase_jobs() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_ALTIBASE", "altibase") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_ALTIBASE not set");
        return;
    };
    with_job(
        c,
        "dbine_test_jobs_alt",
        &[
            "CREATE OR REPLACE PROCEDURE dbine_test_jobs_aproc AS BEGIN NULL; END",
            "CREATE JOB dbine_test_jobs_alt EXEC dbine_test_jobs_aproc START SYSDATE INTERVAL 1 DAY ENABLE",
        ],
        &[
            "DROP JOB dbine_test_jobs_alt",
            "DROP PROCEDURE dbine_test_jobs_aproc",
        ],
        &["DROP JOB dbine_test_jobs_alt"],
        "CREATE JOB",
    )
    .await;
}

const VIRTUOSO_CLEANUP: &[&str] =
    &["DELETE FROM DB.DBA.SYS_SCHEDULED_EVENT WHERE SE_NAME LIKE 'dbine_test_jobs_%'"];

#[tokio::test]
#[ignore]
async fn virtuoso_scheduled_events() {
    let Some(c) = cfg("DBINE_TEST_ODBC_JOBS_VIRTUOSO", "virtuoso") else {
        eprintln!("DBINE_TEST_ODBC_JOBS_VIRTUOSO not set");
        return;
    };
    with_job(
        c,
        "dbine_test_jobs_virt",
        &["INSERT INTO DB.DBA.SYS_SCHEDULED_EVENT (SE_NAME, SE_START, SE_INTERVAL, SE_SQL) VALUES ('dbine_test_jobs_virt', now(), 1440, 'select 1')"],
        VIRTUOSO_CLEANUP,
        VIRTUOSO_CLEANUP,
        "INSERT INTO DB.DBA.SYS_SCHEDULED_EVENT",
    )
    .await;
}

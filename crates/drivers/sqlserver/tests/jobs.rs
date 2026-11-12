//! Server jobs (SQL Server Agent) against real servers: one with the Agent
//! running (`DBINE_TEST_SQLSERVER_AGENT_URL`, a container started with
//! `MSSQL_AGENT_ENABLED=true`; with `TZ=America/New_York` the time zone
//! checks run too), one without it (`DBINE_TEST_SQLSERVER_URL`; the test
//! turns 'Agent XPs' on for a moment and back off) and Babelfish
//! (`DBINE_TEST_BABELFISH_URL`). Skipped without them:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_AGENT_URL='mssql://sa:Pw_12345!@localhost:26131' \
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//! DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:12345678@localhost:25714' \
//!   cargo test -p dbine-driver-sqlserver --test jobs -- --ignored --test-threads=1
//! ```
//!
//! Every job, schedule, category and login a test makes is named
//! `dbine_test_jobs_*` and is dropped at the end, whether the test passed
//! or not.

use dbine_driver::{ConnectionConfig, Error, JobOutcome, QueryOutcome, ServerJob, Session};
use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

const PASSWORD: &str = "Pw_12345!jobs";

fn cfg(env: &str) -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

fn as_login(cfg: &ConnectionConfig, login: &str) -> ConnectionConfig {
    ConnectionConfig {
        username: Some(login.into()),
        password: Some(PASSWORD.into()),
        ..cfg.clone()
    }
}

async fn connect(cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == cfg.driver)
        .unwrap();
    d.connect(cfg, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    let r = s.execute(sql, 100, &mut out).await;
    assert!(
        r.is_ok() && out.errors.is_empty(),
        "{sql}: {r:?} {:?}",
        out.errors
    );
    out
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> serde_json::Value {
    let out = run(s, sql).await;
    out.results
        .iter()
        .rfind(|r| !r.columns.is_empty())
        .expect("a result")
        .rows[0][0]
        .clone()
}

/// Runs the test, then the cleanup on a connection of its own, and only
/// then lets a failed assertion through.
async fn with_cleanup(cfg: &ConnectionConfig, cleanup: String, body: impl Future<Output = ()>) {
    let result = AssertUnwindSafe(body).catch_unwind().await;
    let mut s = connect(cfg).await;
    let mut out = QueryOutcome::default();
    let r = s.execute(&cleanup, 100, &mut out).await;
    if r.is_err() || !out.errors.is_empty() {
        eprintln!("cleanup failed: {r:?} {:?}", out.errors);
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(r.is_ok() && out.errors.is_empty(), "cleanup failed");
}

/// Drops the jobs (stopping them first), the schedules and the category
/// with these exact names, then the logins and their msdb users.
fn cleanup_sql(
    jobs: &[&str],
    schedules: &[&str],
    category: Option<&str>,
    logins: &[&str],
) -> String {
    let mut sql = String::from(
        "SET NOCOUNT ON;\nDECLARE @id int, @spid int, @tries int, @kill nvarchar(40);\n",
    );
    for j in jobs {
        sql.push_str(&format!(
            // The Agent's refusal to stop a job that isn't running reaches
            // the client past TRY/CATCH: stop only a running one.
            "IF EXISTS (SELECT 1 FROM msdb.dbo.sysjobactivity a JOIN msdb.dbo.sysjobs j ON j.job_id = a.job_id
            WHERE j.name = N'{j}' AND a.start_execution_date IS NOT NULL AND a.stop_execution_date IS NULL
              AND a.session_id = (SELECT MAX(session_id) FROM msdb.dbo.syssessions))
    EXEC msdb.dbo.sp_stop_job @job_name = N'{j}';
IF EXISTS (SELECT 1 FROM msdb.dbo.sysjobs WHERE name = N'{j}')
    EXEC msdb.dbo.sp_delete_job @job_name = N'{j}', @delete_unused_schedule = 1;\n"
        ));
    }
    for s in schedules {
        sql.push_str(&format!(
            "WHILE EXISTS (SELECT 1 FROM msdb.dbo.sysschedules WHERE name = N'{s}')
BEGIN
    SELECT TOP (1) @id = schedule_id FROM msdb.dbo.sysschedules WHERE name = N'{s}';
    EXEC msdb.dbo.sp_delete_schedule @schedule_id = @id, @force_delete = 1;
END;\n"
        ));
    }
    if let Some(c) = category {
        sql.push_str(&format!(
            "IF EXISTS (SELECT 1 FROM msdb.dbo.syscategories WHERE name = N'{c}')
    EXEC msdb.dbo.sp_delete_category @class = N'JOB', @name = N'{c}';\n"
        ));
    }
    for l in logins {
        sql.push_str(&format!(
            "SET @tries = 0;
WHILE EXISTS (SELECT 1 FROM sys.dm_exec_sessions WHERE login_name = N'{l}') AND @tries < 20
BEGIN
    SELECT TOP (1) @spid = session_id FROM sys.dm_exec_sessions WHERE login_name = N'{l}';
    SET @kill = N'KILL ' + CAST(@spid AS nvarchar(10));
    BEGIN TRY EXEC (@kill); END TRY BEGIN CATCH END CATCH;
    SET @tries += 1;
END;
IF EXISTS (SELECT 1 FROM msdb.sys.database_principals WHERE name = N'{l}')
    EXEC msdb.sys.sp_executesql N'DROP USER [{l}]';
IF EXISTS (SELECT 1 FROM sys.server_principals WHERE name = N'{l}')
    DROP LOGIN [{l}];\n"
        ));
    }
    sql
}

async fn list(s: &mut Box<dyn Session>) -> dbine_driver::JobList {
    s.jobs().await.expect("jobs")
}

async fn find(s: &mut Box<dyn Session>, name: &str) -> ServerJob {
    list(s)
        .await
        .jobs
        .into_iter()
        .find(|j| j.name == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
}

/// Polls the list until the job reaches the state asked.
async fn wait_for(
    s: &mut Box<dyn Session>,
    name: &str,
    what: &str,
    ok: impl Fn(&ServerJob) -> bool,
) -> ServerJob {
    let start = Instant::now();
    loop {
        let j = find(s, name).await;
        if ok(&j) {
            return j;
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "{name}: still not {what}: {j:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn utc(t: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(t)
        .unwrap_or_else(|e| panic!("{t}: {e}"))
        .with_timezone(&chrono::Utc)
}

fn refused_as_bad_id(r: dbine_driver::Result<impl std::fmt::Debug>) {
    match r {
        Err(Error::Query(m)) => assert!(m.contains("no es un id de job"), "{m}"),
        other => panic!("a malicious id went through: {other:?}"),
    }
}

const MAIN: &str = "dbine_test_jobs_main";
const FAIL: &str = "dbine_test_jobs_fail";
const LONG: &str = "dbine_test_jobs_long";
const DAILY: &str = "dbine_test_jobs_main_daily";
const SHARED: &str = "dbine_test_jobs_shared";
const CATEGORY: &str = "dbine_test_jobs_cat";

fn setup_sql() -> String {
    format!(
        "SET NOCOUNT ON;
EXEC msdb.dbo.sp_add_category @class = N'JOB', @type = N'LOCAL', @name = N'{CATEGORY}';
EXEC msdb.dbo.sp_add_job @job_name = N'{MAIN}', @description = N'DBine test job', @category_name = N'{CATEGORY}', @notify_level_eventlog = 2;
EXEC msdb.dbo.sp_add_jobstep @job_name = N'{MAIN}', @step_name = N'wait', @subsystem = N'TSQL', @database_name = N'master',
     @command = N'WAITFOR DELAY ''00:00:03''', @on_success_action = 3, @retry_attempts = 1, @retry_interval = 1;
EXEC msdb.dbo.sp_add_jobstep @job_name = N'{MAIN}', @step_name = N'print ''two''', @subsystem = N'TSQL', @database_name = N'tempdb',
     @command = N'PRINT ''one''
GO
PRINT ''two''';
EXEC msdb.dbo.sp_add_schedule @schedule_name = N'{DAILY}', @freq_type = 4, @freq_interval = 1, @active_start_time = 30000;
EXEC msdb.dbo.sp_attach_schedule @job_name = N'{MAIN}', @schedule_name = N'{DAILY}';
EXEC msdb.dbo.sp_add_jobserver @job_name = N'{MAIN}';
EXEC msdb.dbo.sp_add_job @job_name = N'{FAIL}';
EXEC msdb.dbo.sp_add_jobstep @job_name = N'{FAIL}', @step_name = N'boom', @subsystem = N'TSQL', @command = N'RAISERROR(''dbine boom'', 16, 1)';
EXEC msdb.dbo.sp_add_schedule @schedule_name = N'{SHARED}', @enabled = 0, @freq_type = 8, @freq_interval = 10,
     @freq_recurrence_factor = 1, @freq_subday_type = 8, @freq_subday_interval = 2, @active_start_time = 80000, @active_end_time = 180000;
EXEC msdb.dbo.sp_attach_schedule @job_name = N'{FAIL}', @schedule_name = N'{SHARED}';
EXEC msdb.dbo.sp_attach_schedule @job_name = N'{MAIN}', @schedule_name = N'{SHARED}';
EXEC msdb.dbo.sp_add_jobserver @job_name = N'{FAIL}';
EXEC msdb.dbo.sp_add_job @job_name = N'{LONG}';
EXEC msdb.dbo.sp_add_jobstep @job_name = N'{LONG}', @step_name = N'sleep', @command = N'WAITFOR DELAY ''00:01:00''';
EXEC msdb.dbo.sp_add_jobserver @job_name = N'{LONG}';"
    )
}

/// History rows dated in July (daylight saving) and January, as the Agent
/// writes them (server local time).
fn old_history_sql() -> String {
    format!(
        "SET NOCOUNT ON;
DECLARE @j uniqueidentifier, @srv sysname;
SELECT @j = job_id FROM msdb.dbo.sysjobs WHERE name = N'{FAIL}';
SET @srv = CAST(SERVERPROPERTY('ServerName') AS sysname);
INSERT INTO msdb.dbo.sysjobhistory (job_id, step_id, step_name, sql_message_id, sql_severity, message, run_status,
       run_date, run_time, run_duration, operator_id_emailed, operator_id_netsent, operator_id_paged, retries_attempted, server)
VALUES (@j, 1, N'boom', 0, 0, N'dbine summer step', 1, 20260701, 30000, 1, 0, 0, 0, 0, @srv),
       (@j, 0, N'(Job outcome)', 0, 0, N'The job succeeded.  The Job was invoked by Schedule 1 (x).', 1, 20260701, 30000, 1, 0, 0, 0, 0, @srv),
       (@j, 0, N'(Job outcome)', 0, 0, N'The job succeeded.  The Job was invoked by Schedule 1 (x).', 1, 20260115, 30000, 1, 0, 0, 0, 0, @srv);"
    )
}

/// List, detail, run now, history (with UTC times), stop, disable/enable,
/// the script (dropping the job and creating it again from it), the
/// session's transaction guard, and ids that aren't ids.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn agent_jobs() {
    let Some(cfg) = cfg("DBINE_TEST_SQLSERVER_AGENT_URL") else {
        return;
    };
    let cleanup = cleanup_sql(&[MAIN, FAIL, LONG], &[DAILY, SHARED], Some(CATEGORY), &[]);
    // Leftovers of an interrupted run.
    run(&mut connect(&cfg).await, &cleanup).await;
    with_cleanup(&cfg, cleanup, async {
        let mut s = connect(&cfg).await;
        run(&mut s, &setup_sql()).await;
        let zone = scalar(&mut s, "SELECT CAST(CURRENT_TIMEZONE_ID() AS nvarchar(128))").await;
        eprintln!("server time zone: {zone}");
        let eastern = zone == "Eastern Standard Time";
        run(&mut s, &old_history_sql()).await;

        // The list (once the Agent worked out the next run).
        wait_for(&mut s, MAIN, "scheduled", |j| j.next_run.is_some()).await;
        let l = list(&mut s).await;
        assert!(l.notes.is_empty(), "{:?}", l.notes);
        let main = l.jobs.iter().find(|j| j.name == MAIN).expect("listed").clone();
        eprintln!("{main:#?}");
        assert_eq!(main.kind, "SQL Server Agent");
        assert_eq!(main.group.as_deref(), Some(CATEGORY));
        assert_eq!(main.description.as_deref(), Some("DBine test job"));
        assert_eq!(main.owner.as_deref(), Some("sa"));
        assert!(main.enabled && !main.running);
        assert_eq!(main.schedule.as_deref(), Some("Todos los días, a las 03:00"));
        assert!(main.next_run.as_deref().is_some_and(|t| t.ends_with(":00Z")), "{main:?}");
        assert!(utc(main.next_run.as_deref().unwrap()) > chrono::Utc::now());
        assert_eq!((main.last_outcome, main.last_started.as_deref()), (None, None));
        assert!(main.can_run && !main.can_stop && main.can_enable && main.has_history && main.has_script);
        let fail = l.jobs.iter().find(|j| j.name == FAIL).expect("listed").clone();
        // Its only schedule is disabled.
        assert_eq!(
            fail.schedule.as_deref(),
            Some("Todas las semanas: lunes y miércoles, cada 2 horas entre las 08:00 y las 18:00 (deshabilitada)")
        );
        assert_eq!(fail.next_run, None);
        let long = l.jobs.iter().find(|j| j.name == LONG).expect("listed").clone();
        assert_eq!(long.schedule, None);

        // The detail.
        let d = s.job_detail(&main.id).await.unwrap();
        eprintln!("{d:#?}");
        assert_eq!(d.job.id, main.id);
        assert_eq!(d.steps.len(), 2);
        assert_eq!(d.steps[0].name, "wait");
        assert_eq!(d.steps[0].kind.as_deref(), Some("T-SQL"));
        assert_eq!(d.steps[0].target.as_deref(), Some("master"));
        assert_eq!(d.steps[0].command.as_deref(), Some("WAITFOR DELAY '00:00:03'"));
        assert_eq!(d.steps[1].name, "print 'two'");
        assert_eq!(d.steps[1].target.as_deref(), Some("tempdb"));
        assert_eq!(d.schedules.len(), 2);
        assert!(d.schedules.contains(&format!("{DAILY}: Todos los días, a las 03:00")), "{:?}", d.schedules);
        let prop = |label: &str| {
            d.properties
                .iter()
                .find(|p| p.label == label)
                .map(|p| p.value.clone())
        };
        assert_eq!(prop("Categoría").as_deref(), Some(CATEGORY));
        assert_eq!(prop("Paso inicial").as_deref(), Some("1. wait"));
        assert_eq!(prop("Registro de eventos").as_deref(), Some("Si falla"));
        assert_eq!(
            prop("1. wait").as_deref(),
            Some("Si se completa: ir al paso siguiente; si falla: salir informando error; reintentos: 1 cada 1 min")
        );
        let created = utc(&prop("Creado").expect("created"));
        assert!(
            (chrono::Utc::now() - created).num_minutes().abs() < 10,
            "created {created} is now, in UTC"
        );

        // Run now: back as soon as the Agent takes it.
        let t = Instant::now();
        s.run_job(&main.id).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(3), "run_job waited for the run");
        let done = wait_for(&mut s, MAIN, "finished", |j| {
            !j.running && j.last_outcome.is_some()
        })
        .await;
        assert_eq!(done.last_outcome, Some(JobOutcome::Succeeded), "{done:?}");
        assert!(done.last_duration_ms.unwrap_or(0) >= 3000, "{done:?}");
        let started = utc(done.last_started.as_deref().unwrap());
        assert!(
            (chrono::Utc::now() - started).num_minutes().abs() < 3,
            "the last start {started} is now, in UTC"
        );
        let runs = s.job_runs(&main.id, 10).await.unwrap();
        eprintln!("{runs:#?}");
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.outcome, JobOutcome::Succeeded);
        assert_eq!(r.trigger.as_deref(), Some("manual"));
        assert!(r.id.is_some() && r.duration_ms.unwrap_or(0) >= 3000);
        assert_eq!(r.started, done.last_started);
        assert!(r.message.as_deref().unwrap_or("").contains("succeeded"), "{r:?}");
        assert_eq!(
            r.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["wait", "print 'two'"]
        );
        assert!(r.steps.iter().all(|s| s.outcome == JobOutcome::Succeeded));
        assert!(r.steps[0].duration_ms.unwrap_or(0) >= 3000);
        // The step ran both batches of its command.
        assert!(r.steps[1].message.as_deref().unwrap_or("").contains("two"), "{r:?}");

        // A failure, with the step's error.
        s.run_job(&fail.id).await.unwrap();
        let failed = wait_for(&mut s, FAIL, "finished", |j| {
            !j.running && j.last_outcome == Some(JobOutcome::Failed)
        })
        .await;
        assert!(!failed.running);
        let runs = s.job_runs(&fail.id, 50).await.unwrap();
        eprintln!("{runs:#?}");
        assert_eq!(runs.len(), 3, "the run and the two written before");
        assert_eq!(runs[0].outcome, JobOutcome::Failed);
        assert_eq!(runs[0].steps.len(), 1);
        assert_eq!(runs[0].steps[0].outcome, JobOutcome::Failed);
        assert!(runs[0].steps[0].message.as_deref().unwrap_or("").contains("dbine boom"), "{runs:?}");
        assert_eq!(s.job_runs(&fail.id, 1).await.unwrap().len(), 1);
        // Each old run with the offset its own date had.
        let jan = runs.iter().find(|r| r.started.as_deref().is_some_and(|t| t.starts_with("2026-01-15"))).expect("January");
        let jul = runs.iter().find(|r| r.started.as_deref().is_some_and(|t| t.starts_with("2026-07-01"))).expect("July");
        assert_eq!(jul.trigger.as_deref(), Some("programada"));
        assert_eq!(jul.steps.len(), 1);
        assert_eq!(jul.steps[0].message.as_deref(), Some("dbine summer step"));
        if eastern {
            assert_eq!(jan.started.as_deref(), Some("2026-01-15T08:00:00Z"));
            assert_eq!(jul.started.as_deref(), Some("2026-07-01T07:00:00Z"));
            assert_eq!(jul.steps[0].started.as_deref(), Some("2026-07-01T07:00:00Z"));
        }

        // Stop: a run of a minute, stopped.
        let t = Instant::now();
        s.run_job(&long.id).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(3));
        let busy = wait_for(&mut s, LONG, "running", |j| j.running).await;
        assert!(busy.can_stop && !busy.can_run, "{busy:?}");
        assert_eq!(busy.last_outcome, Some(JobOutcome::Running));
        let runs = s.job_runs(&long.id, 10).await.unwrap();
        assert_eq!(runs[0].outcome, JobOutcome::Running, "{runs:?}");
        assert!(runs[0].started.is_some() && runs[0].id.is_none());
        s.stop_job(&long.id).await.unwrap();
        let stopped = wait_for(&mut s, LONG, "stopped", |j| !j.running && j.last_outcome.is_some()).await;
        assert_eq!(stopped.last_outcome, Some(JobOutcome::Canceled), "{stopped:?}");
        assert!(t.elapsed() < Duration::from_secs(55), "stopped before it ended");
        assert_eq!(s.job_runs(&long.id, 10).await.unwrap()[0].outcome, JobOutcome::Canceled);
        assert!(s.stop_job(&long.id).await.is_err(), "nothing to stop");

        // Disable and enable.
        s.set_job_enabled(&main.id, false).await.unwrap();
        let off = find(&mut s, MAIN).await;
        assert!(!off.enabled && off.next_run.is_none(), "{off:?}");
        s.set_job_enabled(&main.id, true).await.unwrap();
        assert!(find(&mut s, MAIN).await.enabled);
        wait_for(&mut s, MAIN, "scheduled again", |j| j.next_run.is_some()).await;

        // Not inside the session's own transaction.
        s.set_autocommit(false).await.unwrap();
        run(&mut s, "SELECT COUNT(*) FROM msdb.dbo.sysjobs").await;
        match s.set_job_enabled(&main.id, false).await {
            Err(Error::State(m)) => assert!(m.contains("transacción abierta"), "{m}"),
            other => panic!("ran inside an open transaction: {other:?}"),
        }
        s.rollback().await.unwrap();
        s.set_job_enabled(&main.id, false).await.unwrap();
        assert_eq!(scalar(&mut s, "SELECT @@TRANCOUNT").await, 0, "left no transaction open");
        s.set_job_enabled(&main.id, true).await.unwrap();
        s.set_autocommit(true).await.unwrap();
        assert!(find(&mut s, MAIN).await.enabled, "committed");

        // The script creates it again as it is.
        let script = s.job_script(&main.id).await.unwrap();
        eprintln!("{script}");
        for piece in ["msdb.dbo.sp_add_job", "msdb.dbo.sp_add_jobstep", "msdb.dbo.sp_add_schedule", "msdb.dbo.sp_attach_schedule", "msdb.dbo.sp_add_jobserver"] {
            assert!(script.contains(piece), "{piece}");
        }
        let before = s.job_detail(&main.id).await.unwrap();
        run(&mut s, &format!("EXEC msdb.dbo.sp_delete_job @job_name = N'{MAIN}', @delete_unused_schedule = 1")).await;
        assert!(s.job_detail(&main.id).await.is_err(), "deleted");
        run(&mut s, &script).await;
        let again = find(&mut s, MAIN).await;
        assert_ne!(again.id, main.id);
        let after = s.job_detail(&again.id).await.unwrap();
        assert_eq!(after.steps, before.steps);
        let mut b = before.schedules.clone();
        let mut a = after.schedules.clone();
        b.sort();
        a.sort();
        assert_eq!(a, b);
        let same = |d: &dbine_driver::JobDetail| {
            let j = &d.job;
            (j.group.clone(), j.description.clone(), j.enabled, j.schedule.clone(), j.owner.clone())
        };
        assert_eq!(same(&after), same(&before));
        let labels = |d: &dbine_driver::JobDetail| {
            d.properties
                .iter()
                .filter(|p| !matches!(p.label.as_str(), "Creado" | "Modificado" | "Versión" | "Resultado de la última ejecución" | "Estado"))
                .map(|p| (p.label.clone(), p.value.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(&after), labels(&before));
        assert_eq!(
            scalar(&mut s, &format!("SELECT COUNT(*) FROM msdb.dbo.sysschedules WHERE name = N'{SHARED}'")).await,
            1,
            "the shared schedule is attached again, not copied"
        );

        // Ids that aren't ids never reach the server.
        let evil = "x'; EXEC msdb.dbo.sp_delete_job @job_name = N'dbine_test_jobs_fail'; --";
        refused_as_bad_id(s.job_detail(evil).await);
        refused_as_bad_id(s.job_runs(evil, 5).await);
        refused_as_bad_id(s.run_job(evil).await);
        refused_as_bad_id(s.stop_job(evil).await);
        refused_as_bad_id(s.set_job_enabled(evil, false).await);
        refused_as_bad_id(s.job_script(evil).await);
        refused_as_bad_id(s.run_job(&format!("{}'", &fail.id[..35])).await);
        assert!(find(&mut s, FAIL).await.id == fail.id, "still there");
        match s.job_detail("00000000-0000-0000-0000-000000000000").await {
            Err(Error::Query(m)) => assert!(m.contains("no existe"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(s.run_job("00000000-0000-0000-0000-000000000000").await.is_err());
    })
    .await;
}

const ROLES_SA: &str = "dbine_test_jobs_roles_sa";
const ROLES_OWN: &str = "dbine_test_jobs_roles_own";
const USER: &str = "dbine_test_jobs_user";
const READER: &str = "dbine_test_jobs_reader";
const NOBODY: &str = "dbine_test_jobs_nobody";

/// What each Agent role sees and may do: SQLAgentUserRole only its own
/// jobs (and says the list is partial), SQLAgentReaderRole every job but
/// runs none of another's, a login without roles nothing (with a note).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn agent_roles() {
    let Some(cfg) = cfg("DBINE_TEST_SQLSERVER_AGENT_URL") else {
        return;
    };
    let cleanup = cleanup_sql(&[ROLES_SA, ROLES_OWN], &[], None, &[USER, READER, NOBODY]);
    run(&mut connect(&cfg).await, &cleanup).await;
    with_cleanup(&cfg, cleanup, async {
        let mut admin = connect(&cfg).await;
        run(
            &mut admin,
            &format!(
                "SET NOCOUNT ON;
CREATE LOGIN {USER} WITH PASSWORD = N'{PASSWORD}', CHECK_POLICY = OFF;
CREATE LOGIN {READER} WITH PASSWORD = N'{PASSWORD}', CHECK_POLICY = OFF;
CREATE LOGIN {NOBODY} WITH PASSWORD = N'{PASSWORD}', CHECK_POLICY = OFF;
EXEC msdb.sys.sp_executesql N'CREATE USER {USER} FOR LOGIN {USER}; ALTER ROLE SQLAgentUserRole ADD MEMBER {USER};
    CREATE USER {READER} FOR LOGIN {READER}; ALTER ROLE SQLAgentReaderRole ADD MEMBER {READER};';
EXEC msdb.dbo.sp_add_job @job_name = N'{ROLES_SA}';
EXEC msdb.dbo.sp_add_jobstep @job_name = N'{ROLES_SA}', @step_name = N'one', @command = N'SELECT 1';
EXEC msdb.dbo.sp_add_jobserver @job_name = N'{ROLES_SA}';
EXEC msdb.dbo.sp_add_job @job_name = N'{ROLES_OWN}', @owner_login_name = N'{USER}';
EXEC msdb.dbo.sp_add_jobstep @job_name = N'{ROLES_OWN}', @step_name = N'one', @command = N'SELECT 1';
EXEC msdb.dbo.sp_add_jobserver @job_name = N'{ROLES_OWN}';"
            ),
        )
        .await;
        let sa_job = find(&mut admin, ROLES_SA).await;

        // SQLAgentUserRole: its own job only, and it says so.
        let mut user = connect(&as_login(&cfg, USER)).await;
        let l = list(&mut user).await;
        eprintln!("user: {l:#?}");
        assert!(l.jobs.iter().all(|j| j.name != ROLES_SA));
        let own = l.jobs.iter().find(|j| j.name == ROLES_OWN).expect("its own job").clone();
        assert!(l.notes.iter().any(|n| n.contains("dueño")), "{:?}", l.notes);
        assert!(own.can_run && own.can_enable && own.has_history && own.has_script, "{own:?}");
        assert!(user.job_detail(&sa_job.id).await.is_err(), "another's job");
        assert!(user.run_job(&sa_job.id).await.is_err());
        user.run_job(&own.id).await.unwrap();
        wait_for(&mut user, ROLES_OWN, "finished", |j| !j.running && j.last_outcome.is_some()).await;
        let runs = user.job_runs(&own.id, 5).await.unwrap();
        assert_eq!(runs[0].outcome, JobOutcome::Succeeded, "{runs:?}");
        user.set_job_enabled(&own.id, false).await.unwrap();
        assert!(!find(&mut user, ROLES_OWN).await.enabled);
        assert!(user.job_script(&own.id).await.unwrap().contains(ROLES_OWN));
        drop(user);

        // SQLAgentReaderRole: every job, history included; runs none of
        // another's.
        let mut reader = connect(&as_login(&cfg, READER)).await;
        let l = list(&mut reader).await;
        eprintln!("reader: {l:#?}");
        assert!(l.notes.is_empty(), "{:?}", l.notes);
        let theirs = l.jobs.iter().find(|j| j.name == ROLES_SA).expect("every job").clone();
        assert!(l.jobs.iter().any(|j| j.name == ROLES_OWN));
        assert!(!theirs.can_run && !theirs.can_stop && !theirs.can_enable, "{theirs:?}");
        assert!(theirs.has_history && theirs.has_script);
        assert!(reader.job_runs(&theirs.id, 5).await.is_ok());
        assert!(reader.job_detail(&theirs.id).await.is_ok());
        assert!(reader.run_job(&theirs.id).await.is_err(), "the server refuses it too");
        drop(reader);

        // No Agent role: nothing, and why.
        let mut nobody = connect(&as_login(&cfg, NOBODY)).await;
        let l = list(&mut nobody).await;
        eprintln!("nobody: {l:#?}");
        assert!(l.jobs.is_empty());
        assert_eq!(l.notes.len(), 1);
        for role in ["SQLAgentUserRole", "SQLAgentReaderRole", "SQLAgentOperatorRole"] {
            assert!(l.notes[0].contains(role), "{:?}", l.notes);
        }
        assert!(nobody.job_detail(&sa_job.id).await.is_err());
    })
    .await;
}

const NO_AGENT: &str = "dbine_test_jobs_noagent";
const NO_AGENT_DAILY: &str = "dbine_test_jobs_noagent_daily";

fn config_sql(name: &str) -> String {
    format!("SELECT CAST(value_in_use AS int) FROM sys.configurations WHERE name = N'{name}'")
}

/// Sets 'Agent XPs', then puts 'show advanced options' back as it was.
fn agent_xps_sql(on: u8, advanced: &serde_json::Value) -> String {
    format!(
        "EXEC sp_configure 'show advanced options', 1; RECONFIGURE;
EXEC sp_configure 'Agent XPs', {on}; RECONFIGURE;
EXEC sp_configure 'show advanced options', {advanced}; RECONFIGURE;
"
    )
}

/// A server whose Agent isn't running: the jobs are listed with a note,
/// can't be run, have no next run, and can still be disabled and enabled.
/// Also with 'Agent XPs' on but no Agent (as after it crashed).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn agent_stopped() {
    let Some(cfg) = cfg("DBINE_TEST_SQLSERVER_URL") else {
        return;
    };
    let mut s = connect(&cfg).await;
    if scalar(&mut s, &config_sql("Agent XPs")).await != 0 {
        eprintln!("the Agent runs on this server; skipping");
        return;
    }
    let advanced = scalar(&mut s, &config_sql("show advanced options")).await;
    // 'Agent XPs' off again first: msdb's procedures behave as before.
    let cleanup = format!(
        "{}{}",
        agent_xps_sql(0, &advanced),
        cleanup_sql(&[NO_AGENT], &[NO_AGENT_DAILY], None, &[])
    );
    run(&mut s, &cleanup).await;
    drop(s);
    with_cleanup(&cfg, cleanup, async {
        let mut s = connect(&cfg).await;
        run(
            &mut s,
            &format!(
                "SET NOCOUNT ON;
EXEC msdb.dbo.sp_add_job @job_name = N'{NO_AGENT}';
EXEC msdb.dbo.sp_add_jobstep @job_name = N'{NO_AGENT}', @step_name = N'one', @command = N'SELECT 1';
EXEC msdb.dbo.sp_add_schedule @schedule_name = N'{NO_AGENT_DAILY}', @freq_type = 4, @freq_interval = 1, @active_start_time = 30000;
EXEC msdb.dbo.sp_attach_schedule @job_name = N'{NO_AGENT}', @schedule_name = N'{NO_AGENT_DAILY}';
EXEC msdb.dbo.sp_add_jobserver @job_name = N'{NO_AGENT}';
-- The next run an Agent worked out before it stopped.
UPDATE js SET next_run_date = 20300101, next_run_time = 30000
  FROM msdb.dbo.sysjobschedules js JOIN msdb.dbo.sysjobs j ON j.job_id = js.job_id
 WHERE j.name = N'{NO_AGENT}';"
            ),
        )
        .await;
        let l = list(&mut s).await;
        eprintln!("{l:#?}");
        assert!(
            l.notes.iter().any(|n| n.contains("no está en ejecución")),
            "{:?}",
            l.notes
        );
        let j = l
            .jobs
            .iter()
            .find(|j| j.name == NO_AGENT)
            .expect("listed")
            .clone();
        assert!(
            !j.can_run && !j.can_stop && j.can_enable && j.has_history,
            "{j:?}"
        );
        assert_eq!(j.schedule.as_deref(), Some("Todos los días, a las 03:00"));
        assert_eq!(j.next_run, None, "nothing starts on schedule: {j:?}");

        // 'Agent XPs' on, but no Agent: still not running.
        run(&mut s, &agent_xps_sql(1, &advanced)).await;
        let on = list(&mut s).await;
        run(&mut s, &agent_xps_sql(0, &advanced)).await;
        eprintln!("{on:#?}");
        assert!(
            on.notes.iter().any(|n| n.contains("no está en ejecución")),
            "{:?}",
            on.notes
        );
        let j2 = on.jobs.iter().find(|j| j.name == NO_AGENT).expect("listed");
        assert!(!j2.can_run && !j2.can_stop && j2.can_enable, "{j2:?}");
        assert_eq!(j2.next_run, None);
        assert_eq!(scalar(&mut s, &config_sql("Agent XPs")).await, 0);
        assert_eq!(scalar(&mut s, &config_sql("show advanced options")).await, advanced);

        assert!(s.run_job(&j.id).await.is_err());
        s.set_job_enabled(&j.id, false).await.unwrap();
        assert!(!find(&mut s, NO_AGENT).await.enabled);
        s.set_job_enabled(&j.id, true).await.unwrap();
        assert!(find(&mut s, NO_AGENT).await.enabled);
        assert_eq!(s.job_detail(&j.id).await.unwrap().steps.len(), 1);
        assert!(s.job_runs(&j.id, 10).await.unwrap().is_empty());
        assert!(s.job_script(&j.id).await.unwrap().contains(NO_AGENT));
    })
    .await;
}

/// Babelfish (`DBINE_TEST_BABELFISH_URL`) through the `babelfish` driver:
/// no jobs at all. Reached through the `sqlserver` driver (which needs TLS,
/// and the test container has none) it says it has no Agent: see the
/// crate's `jobs::tests::babelfish_as_sqlserver_live`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn babelfish_has_no_jobs() {
    let Some(cfg) = cfg("DBINE_TEST_BABELFISH_URL") else {
        return;
    };
    let mut b = connect(&ConnectionConfig {
        driver: "babelfish".into(),
        ..cfg
    })
    .await;
    assert!(matches!(b.jobs().await, Err(Error::Unsupported(_))));
    let id = "35b8fd0b-0208-4795-a5b9-0fdc971a8414";
    assert!(matches!(b.job_detail(id).await, Err(Error::Unsupported(_))));
    assert!(matches!(b.run_job(id).await, Err(Error::Unsupported(_))));
}

/// SQL Server and Azure SQL have them; a Fabric warehouse and Babelfish
/// don't.
#[test]
fn capabilities_follow_the_engine() {
    for d in dbine_driver_sqlserver::drivers() {
        let id = d.info().id;
        assert_eq!(
            d.capabilities().jobs,
            matches!(id, "sqlserver" | "azuresql"),
            "{id}"
        );
    }
}

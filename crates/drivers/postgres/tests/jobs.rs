//! Server jobs against real servers. Each test reads `DBINE_TEST_<NAME>_URL`
//! (`postgres://user:pass@host:port/db`) and is skipped without it. Every
//! test creates its own jobs, named `dbine_test_jobs_…`, and removes them at
//! the end, also when an assertion fails (the body runs under
//! `catch_unwind` and the cleanup runs before the panic goes on).
//!
//! pg_cron and pgAgent need a server with both (pgAgent's daemon running
//! for "run now"):
//!
//! ```sh
//! # FROM postgres:16 + apt-get install postgresql-16-cron pgagent
//! docker build -t dbine-test-pgcron-img .
//! docker run -d --name dbine-test-pgcron -e POSTGRES_PASSWORD=pw -p 25040:5432 dbine-test-pgcron-img \
//!   -c shared_preload_libraries=pg_cron -c cron.database_name=postgres
//! docker exec dbine-test-pgcron psql -U postgres -c 'CREATE EXTENSION pg_cron' -c 'CREATE EXTENSION pgagent'
//! docker exec -d dbine-test-pgcron pgagent -f -t 1 hostaddr=127.0.0.1 dbname=postgres user=postgres password=pw
//!
//! DBINE_TEST_PGCRON_URL=postgres://postgres:pw@localhost:25040/postgres \
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_TIMESCALEDB_URL=postgres://postgres:pw@localhost:25015/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_CLOUDBERRY_URL=postgres://gpadmin:pw@localhost:25017/demo \
//! DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres' \
//!   cargo test -p dbine-driver-postgres --test jobs -- --ignored --nocapture
//! ```
//!
//! The CockroachDB test backs up into `userfile:///dbine_test_jobs_backup`
//! (tables in `defaultdb`) and deletes those files at the end; it speeds up
//! the scheduler (`jobs.scheduler.pace`) while it runs and puts it back.
//! The TimescaleDB test turns `timescaledb.enable_job_execution_logging` on
//! for a while (`ALTER SYSTEM`) when the server has it at its default.

use dbine_driver::{ConnectionConfig, Error, JobOutcome, QueryOutcome, ServerJob, Session};
use futures::FutureExt;
use serde_json::Value;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn connect(driver: &str, cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    assert!(d.capabilities().jobs, "{driver} declares jobs");
    d.connect(cfg, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.map(|_| out)
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    if let Err(e) = run(s, sql).await {
        panic!("{sql}: {e}");
    }
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let o = run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    match o.results.iter().rev().find(|r| !r.rows.is_empty()) {
        Some(r) => match &r.rows[0][0] {
            Value::String(v) => v.clone(),
            Value::Null => String::new(),
            v => v.to_string(),
        },
        None => String::new(),
    }
}

/// Run `body`, then `cleanup` whatever happened, then fail if `body` did.
async fn guarded<B, C>(body: B, cleanup: C)
where
    B: Future<Output = ()>,
    C: Future<Output = ()>,
{
    let outcome = AssertUnwindSafe(body).catch_unwind().await;
    cleanup.await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// The job named `name` (TimescaleDB adds ` [<id>]` to the names it's given).
fn find<'a>(list: &'a [ServerJob], name: &str) -> &'a ServerJob {
    list.iter()
        .find(|j| j.name == name || j.name.starts_with(&format!("{name} [")))
        .unwrap_or_else(|| panic!("{name} in {list:#?}"))
}

async fn job(s: &mut Box<dyn Session>, name: &str) -> ServerJob {
    find(&s.jobs().await.unwrap().jobs, name).clone()
}

/// Polls `check` (an expression that may `.await`) every half second until
/// it's true or `secs` pass.
macro_rules! wait_for {
    ($secs:expr, $what:expr, $check:expr) => {{
        let until = Instant::now() + Duration::from_secs($secs);
        loop {
            if $check {
                break;
            }
            assert!(Instant::now() < until, "timed out waiting for {}", $what);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }};
}

/// Ids that weren't written by the driver are refused before any SQL.
async fn refuses_bad_ids(s: &mut Box<dyn Session>, real: &str) {
    let bad = [
        format!("{real}; DROP TABLE x"),
        format!("{real} OR 1=1"),
        format!("{real}'--"),
        "pg_cron:0".to_string(),
        "pgagent:-1".to_string(),
        "timescaledb:1)--".to_string(),
        "crdb_schedule:1 UNION SELECT 1".to_string(),
        "x".repeat(10_000),
    ];
    for id in &bad {
        let shown: String = id.chars().take(40).collect();
        for e in [
            s.job_detail(id).await.err(),
            s.job_runs(id, 5).await.err(),
            s.run_job(id).await.err(),
            s.stop_job(id).await.err(),
            s.set_job_enabled(id, false).await.err(),
            s.job_script(id).await.err(),
        ] {
            let e = e.unwrap_or_else(|| panic!("{shown} was taken"));
            assert!(
                matches!(&e, Error::Query(m) if m.contains("no es un id de job")),
                "{shown}: {e}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL without any scheduler

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_without_schedulers_says_why() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_POSTGRES_URL") else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let mut s = connect("postgres", &cfg).await;
    let list = s.jobs().await.unwrap();
    eprintln!("{list:#?}");
    assert!(list.jobs.is_empty());
    assert_eq!(list.notes.len(), 1);
    for word in ["pg_cron", "pgAgent", "TimescaleDB"] {
        assert!(list.notes[0].contains(word), "{list:?}");
    }
    let e = s.job_detail("pg_cron:1").await.unwrap_err();
    assert!(e.to_string().contains("pg_cron no está instalado"), "{e}");
    refuses_bad_ids(&mut s, "pg_cron:1").await;
}

// ---------------------------------------------------------------------------
// pg_cron

/// The role the pg_cron and Cloudberry tests create for another user's jobs.
const ROLE: &str = "dbine_test_jobs_role";

/// Unschedules the test's jobs, then deletes their runs until none show up
/// any more (a run already under way, or YugabyteDB's scheduler catching
/// up, can still write one after the job is gone). The commands carry
/// `dbine_test_jobs` so their runs are found without their job. Then the
/// test's role goes, with the grants it got.
async fn cron_cleanup(s: &mut Box<dyn Session>) {
    exec(
        s,
        "SELECT count(cron.unschedule(jobid)) FROM cron.job WHERE jobname LIKE 'dbine\\_test\\_jobs\\_%'",
    )
    .await;
    exec(
        s,
        &format!(
            "DO $$ BEGIN
               IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{ROLE}') THEN
                 EXECUTE 'DROP OWNED BY {ROLE}';
                 EXECUTE 'DROP ROLE {ROLE}';
               END IF;
             END $$"
        ),
    )
    .await;
    let until = Instant::now() + Duration::from_secs(90);
    loop {
        exec(
            s,
            "DELETE FROM cron.job_run_details WHERE command LIKE '%dbine\\_test\\_jobs%'",
        )
        .await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let left = scalar(
            s,
            "SELECT count(*) FROM cron.job_run_details WHERE command LIKE '%dbine\\_test\\_jobs%'",
        )
        .await;
        if left == "0" || Instant::now() > until {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pg_cron() {
    cron_on("postgres", "DBINE_TEST_PGCRON_URL").await;
}

/// YugabyteDB with `enable_pg_cron` on the masters and tservers:
///
/// ```sh
/// docker run -d --name dbine-test-yugabyte-cron -p 25041:5433 yugabytedb/yugabyte:latest \
///   bin/yugabyted start --background=false \
///   --tserver_flags="enable_pg_cron=true,ysql_cron_database_name=yugabyte" --master_flags="enable_pg_cron=true"
/// docker exec dbine-test-yugabyte-cron bash -c 'bin/ysqlsh -h $(hostname) -c "CREATE EXTENSION pg_cron"'
/// DBINE_TEST_YUGABYTEDB_CRON_URL=postgres://yugabyte@localhost:25041/yugabyte
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn yugabytedb_pg_cron() {
    cron_on("yugabytedb", "DBINE_TEST_YUGABYTEDB_CRON_URL").await;
}

async fn cron_on(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut s = connect(driver, &cfg).await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT count(*) FROM pg_extension WHERE extname = 'pg_cron'"
        )
        .await,
        "1",
        "the server needs pg_cron (see the top of this file)"
    );
    cron_cleanup(&mut s).await;
    let body = async {
        let ok = scalar(
            &mut s,
            "SELECT cron.schedule('dbine_test_jobs_cron', '1 seconds', 'SELECT 1 AS dbine_test_jobs')",
        )
        .await;
        let bad = scalar(
            &mut s,
            "SELECT cron.schedule('dbine_test_jobs_cron_fail', '1 seconds', 'SELECT 1/0 AS dbine_test_jobs')",
        )
        .await;
        let (ok, bad) = (format!("pg_cron:{ok}"), format!("pg_cron:{bad}"));

        // The list, and the last runs once they happen.
        let j = job(&mut s, "dbine_test_jobs_cron").await;
        eprintln!("{j:#?}");
        assert_eq!(j.id, ok);
        assert_eq!(j.kind, "pg_cron");
        assert_eq!(j.schedule.as_deref(), Some("1 seconds"));
        assert_eq!(j.group.as_deref(), Some(cfg.database.as_str()));
        assert!(j.enabled && j.can_enable && j.has_history && j.has_script);
        assert!(!j.can_run && !j.can_stop);
        // YugabyteDB picks up new jobs within a minute or so.
        wait_for!(150, "both jobs to run", {
            let list = s.jobs().await.unwrap().jobs;
            find(&list, "dbine_test_jobs_cron").last_outcome == Some(JobOutcome::Succeeded)
                && find(&list, "dbine_test_jobs_cron_fail").last_outcome == Some(JobOutcome::Failed)
        });
        let j = job(&mut s, "dbine_test_jobs_cron").await;
        assert!(
            j.last_started
                .as_deref()
                .is_some_and(|t| t.ends_with('Z') && t.len() == 20),
            "{j:?}"
        );

        // Detail.
        let d = s.job_detail(&ok).await.unwrap();
        assert_eq!(d.job.id, ok);
        assert_eq!(d.steps.len(), 1);
        assert_eq!(
            d.steps[0].command.as_deref(),
            Some("SELECT 1 AS dbine_test_jobs")
        );
        assert!(d.properties.iter().any(|p| p.label == "Base de datos"));

        // History, newest first, with the failure's message.
        let runs = s.job_runs(&ok, 3).await.unwrap();
        assert!(!runs.is_empty() && runs.len() <= 3, "{runs:#?}");
        assert!(
            runs.windows(2).all(|w| w[0].started >= w[1].started),
            "{runs:#?}"
        );
        assert!(runs.iter().any(|r| r.outcome == JobOutcome::Succeeded));
        let failed = s.job_runs(&bad, 2).await.unwrap();
        eprintln!("{failed:#?}");
        let f = failed
            .iter()
            .find(|r| r.outcome == JobOutcome::Failed)
            .unwrap();
        assert!(
            f.message
                .as_deref()
                .unwrap_or("")
                .contains("division by zero"),
            "{f:?}"
        );

        // No run now, no stop.
        assert!(matches!(s.run_job(&ok).await, Err(Error::Unsupported(_))));
        assert!(matches!(s.stop_job(&ok).await, Err(Error::Unsupported(_))));

        // Disable and enable.
        s.set_job_enabled(&bad, false).await.unwrap();
        assert!(!job(&mut s, "dbine_test_jobs_cron_fail").await.enabled);
        assert_eq!(
            scalar(
                &mut s,
                &format!(
                    "SELECT active::text FROM cron.job WHERE jobid = {}",
                    &bad[8..]
                )
            )
            .await,
            "false"
        );
        s.set_job_enabled(&bad, true).await.unwrap();
        assert!(job(&mut s, "dbine_test_jobs_cron_fail").await.enabled);

        // The script schedules it again as it is (inactive: through
        // schedule_in_database, which takes the user too).
        s.set_job_enabled(&ok, false).await.unwrap();
        let script = s.job_script(&ok).await.unwrap();
        eprintln!("{script}");
        assert!(
            script.contains(
                "cron.schedule_in_database(E'dbine_test_jobs_cron', E'1 seconds', $cmd$SELECT 1 AS dbine_test_jobs$cmd$, "
            ) && script.contains("active := false"),
            "{script}"
        );
        exec(&mut s, &format!("SELECT cron.unschedule({})", &ok[8..])).await;
        exec(&mut s, &script).await;
        let again = job(&mut s, "dbine_test_jobs_cron").await;
        assert_ne!(again.id, ok);
        assert_eq!(again.schedule.as_deref(), Some("1 seconds"));
        assert!(!again.enabled, "the script keeps it inactive");

        refuses_bad_ids(&mut s, &bad).await;
        assert!(s.job_detail(&bad).await.is_ok(), "the job is still there");
        assert!(s.job_detail("pg_cron:999999999").await.is_err());
        assert!(s.job_detail("crdb_schedule:1").await.is_err());

        // Another user's job with the name of one of the runner's: its
        // script recreates that user's job and leaves the runner's alone.
        let me = cfg.username.clone().unwrap_or_default();
        exec(
            &mut s,
            &format!("CREATE ROLE {ROLE} LOGIN PASSWORD 'dbine_test_pw'"),
        )
        .await;
        exec(&mut s, &format!("GRANT USAGE ON SCHEMA cron TO {ROLE}")).await;
        let theirs = scalar(
            &mut s,
            &format!(
                "SELECT cron.schedule_in_database('dbine_test_jobs_shared', '0 0 1 1 *',
                        'SELECT 2 AS dbine_test_jobs', current_database(), '{ROLE}')"
            ),
        )
        .await;
        let mine = scalar(
            &mut s,
            "SELECT cron.schedule('dbine_test_jobs_shared', '0 0 1 1 *', 'SELECT 1 AS dbine_test_jobs')",
        )
        .await;
        let theirs = format!("pg_cron:{theirs}");
        let script = s.job_script(&theirs).await.unwrap();
        eprintln!("{script}");
        assert!(
            script.contains(&format!("username := E'{ROLE}'")),
            "{script}"
        );
        exec(&mut s, &format!("SELECT cron.unschedule({})", &theirs[8..])).await;
        exec(&mut s, &script).await;
        let shared = |user: &str| {
            format!(
                "SELECT command FROM cron.job WHERE jobname = 'dbine_test_jobs_shared' AND username = '{user}'"
            )
        };
        assert_eq!(
            scalar(&mut s, &shared(ROLE)).await,
            "SELECT 2 AS dbine_test_jobs"
        );
        assert_eq!(
            scalar(&mut s, &shared(&me)).await,
            "SELECT 1 AS dbine_test_jobs",
            "the runner's own job is untouched"
        );
        assert_eq!(
            scalar(
                &mut s,
                &format!("SELECT jobid::text FROM cron.job WHERE jobname = 'dbine_test_jobs_shared' AND username = '{me}'")
            )
            .await,
            mine
        );

        // That user reads the list (pg_cron's settings are superuser-only)
        // and sees only their job; they change it once they may run
        // cron.alter_job, which pg_cron grants to its owner only.
        let as_role = ConnectionConfig {
            username: Some(ROLE.into()),
            password: Some("dbine_test_pw".into()),
            ..cfg.clone()
        };
        let mut r = connect(driver, &as_role).await;
        let list = r.jobs().await.unwrap();
        eprintln!("{list:#?}");
        assert_eq!(list.jobs.len(), 1, "{list:#?}");
        assert_eq!(list.jobs[0].owner.as_deref(), Some(ROLE));
        assert!(
            list.notes.iter().any(|n| n.contains("solo sus jobs")),
            "{list:?}"
        );
        let own = list.jobs[0].clone();
        assert!(!own.can_enable, "{own:?}");
        assert!(r.set_job_enabled(&own.id, false).await.is_err());
        exec(
            &mut s,
            &format!(
                "GRANT EXECUTE ON FUNCTION cron.alter_job(bigint, text, text, text, text, boolean) TO {ROLE}"
            ),
        )
        .await;
        let own = job(&mut r, "dbine_test_jobs_shared").await;
        assert!(own.can_enable && own.enabled);
        r.set_job_enabled(&own.id, false).await.unwrap();
        assert!(!job(&mut r, "dbine_test_jobs_shared").await.enabled);
        drop(r);
    };
    guarded(body, async {
        let mut c = connect(driver, &cfg).await;
        cron_cleanup(&mut c).await;
    })
    .await;
    assert_eq!(
        scalar(
            &mut s,
            &format!("SELECT count(*) FROM pg_roles WHERE rolname = '{ROLE}'")
        )
        .await,
        "0"
    );
    assert_eq!(
        scalar(
            &mut s,
            "SELECT count(*) FROM cron.job WHERE jobname LIKE 'dbine\\_test\\_jobs\\_%'"
        )
        .await,
        "0"
    );

    // From another database, the list says where pg_cron is.
    let other = ConnectionConfig {
        database: "template1".into(),
        ..cfg.clone()
    };
    let mut t = connect(driver, &other).await;
    let list = t.jobs().await.unwrap();
    eprintln!("{list:#?}");
    assert!(list.jobs.is_empty());
    assert!(
        list.notes
            .iter()
            .any(|n| n.contains(&format!("«{}»", cfg.database))),
        "{list:?}"
    );
}

// ---------------------------------------------------------------------------
// Apache Cloudberry's tasks (CREATE TASK)

const TASK_CLEANUP: &str = "DROP TASK IF EXISTS dbine_test_jobs_task;
     DROP TASK IF EXISTS dbine_test_jobs_task_fail;
     DROP TASK IF EXISTS dbine_test_jobs_shared";

/// The runner's tasks, then the test role's (dropped as that role: `DROP
/// TASK` only finds the current user's) and the role.
async fn task_cleanup(s: &mut Box<dyn Session>) {
    exec(s, TASK_CLEANUP).await;
    let role = scalar(
        s,
        &format!("SELECT count(*) FROM pg_roles WHERE rolname = '{ROLE}'"),
    )
    .await;
    if role == "1" {
        exec(
            s,
            &format!(
                "BEGIN; SET LOCAL ROLE {ROLE}; DROP TASK IF EXISTS dbine_test_jobs_shared; COMMIT"
            ),
        )
        .await;
        exec(s, &format!("DROP ROLE {ROLE}")).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cloudberry_tasks() {
    let Some(cfg) = cfg("cloudberry", "DBINE_TEST_CLOUDBERRY_URL") else {
        eprintln!("DBINE_TEST_CLOUDBERRY_URL not set; skipping");
        return;
    };
    let mut s = connect("cloudberry", &cfg).await;
    task_cleanup(&mut s).await;
    let body = async {
        exec(
            &mut s,
            "CREATE TASK dbine_test_jobs_task SCHEDULE '1 second' AS 'SELECT 1'",
        )
        .await;
        exec(
            &mut s,
            "CREATE TASK dbine_test_jobs_task_fail SCHEDULE '1 second' AS 'SELECT 1/0'",
        )
        .await;
        let list = s.jobs().await.unwrap();
        eprintln!("{list:#?}");
        let j = find(&list.jobs, "dbine_test_jobs_task").clone();
        let bad = find(&list.jobs, "dbine_test_jobs_task_fail").id.clone();
        assert!(j.id.starts_with("task:"));
        assert_eq!(j.kind, "Task");
        assert_eq!(j.schedule.as_deref(), Some("1 second"));
        assert!(j.enabled && j.can_enable && j.has_history && j.has_script);
        assert!(!j.can_run && !j.can_stop);
        wait_for!(30, "both tasks to run", {
            let list = s.jobs().await.unwrap().jobs;
            find(&list, "dbine_test_jobs_task").last_outcome == Some(JobOutcome::Succeeded)
                && find(&list, "dbine_test_jobs_task_fail").last_outcome == Some(JobOutcome::Failed)
        });
        let d = s.job_detail(&j.id).await.unwrap();
        assert_eq!(d.steps[0].command.as_deref(), Some("SELECT 1"));
        let runs = s.job_runs(&bad, 3).await.unwrap();
        eprintln!("{runs:#?}");
        assert_eq!(runs[0].outcome, JobOutcome::Failed);
        assert!(runs[0]
            .message
            .as_deref()
            .unwrap_or("")
            .contains("division by zero"));
        assert!(matches!(s.run_job(&j.id).await, Err(Error::Unsupported(_))));

        s.set_job_enabled(&j.id, false).await.unwrap();
        assert!(!job(&mut s, "dbine_test_jobs_task").await.enabled);
        let script = s.job_script(&j.id).await.unwrap();
        eprintln!("{script}");
        assert!(
            script.contains("CREATE TASK \"dbine_test_jobs_task\" SCHEDULE E'1 second'"),
            "{script}"
        );
        assert!(script.contains("NOT ACTIVE"));
        exec(&mut s, "DROP TASK dbine_test_jobs_task").await;
        exec(&mut s, &script).await;
        let again = job(&mut s, "dbine_test_jobs_task").await;
        assert_ne!(again.id, j.id);
        assert!(!again.enabled);
        s.set_job_enabled(&again.id, true).await.unwrap();
        assert!(job(&mut s, "dbine_test_jobs_task").await.enabled);

        refuses_bad_ids(&mut s, &bad).await;
        assert!(s.job_detail(&bad).await.is_ok(), "the task is still there");

        // Another user's task with the name of one of the runner's: ALTER
        // TASK would reach the runner's, so only its owner changes it, and
        // its script creates it as that user.
        let me = cfg.username.clone().unwrap_or_default();
        exec(&mut s, &format!("CREATE ROLE {ROLE} LOGIN")).await;
        exec(
            &mut s,
            &format!(
                "CREATE TASK dbine_test_jobs_shared SCHEDULE '0 3 * * *' USER {ROLE} AS 'SELECT 2'"
            ),
        )
        .await;
        exec(
            &mut s,
            "CREATE TASK dbine_test_jobs_shared SCHEDULE '0 4 * * *' AS 'SELECT 1'",
        )
        .await;
        let list = s.jobs().await.unwrap().jobs;
        let shared = |owner: &str| {
            list.iter()
                .find(|j| j.name == "dbine_test_jobs_shared" && j.owner.as_deref() == Some(owner))
                .unwrap_or_else(|| panic!("{owner}'s task in {list:#?}"))
                .clone()
        };
        let (theirs, mine) = (shared(ROLE), shared(&me));
        assert!(!theirs.can_enable && mine.can_enable);
        assert!(matches!(
            s.set_job_enabled(&theirs.id, false).await,
            Err(Error::Query(m)) if m.contains(ROLE)
        ));
        let states = "SELECT string_agg(username || ':' || active::text || ':' || command, ', ' ORDER BY username)
                      FROM pg_task WHERE jobname = 'dbine_test_jobs_shared'";
        let all_on = format!("{ROLE}:true:SELECT 2, {me}:true:SELECT 1");
        assert_eq!(scalar(&mut s, states).await, all_on, "nothing changed");
        exec(
            &mut s,
            &format!("BEGIN; SET LOCAL ROLE {ROLE}; ALTER TASK dbine_test_jobs_shared NOT ACTIVE; COMMIT"),
        )
        .await;
        let script = s.job_script(&theirs.id).await.unwrap();
        eprintln!("{script}");
        assert!(
            script.contains(&format!("SET LOCAL ROLE \"{ROLE}\"")),
            "{script}"
        );
        exec(
            &mut s,
            &format!("BEGIN; SET LOCAL ROLE {ROLE}; DROP TASK dbine_test_jobs_shared; COMMIT"),
        )
        .await;
        exec(&mut s, &script).await;
        assert_eq!(
            scalar(&mut s, states).await,
            format!("{ROLE}:false:SELECT 2, {me}:true:SELECT 1"),
            "theirs again, inactive; the runner's untouched"
        );
    };
    guarded(body, async {
        let mut c = connect("cloudberry", &cfg).await;
        task_cleanup(&mut c).await;
    })
    .await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT count(*) FROM pg_task WHERE jobname LIKE 'dbine\\_test\\_jobs\\_%'"
        )
        .await,
        "0"
    );
}

// ---------------------------------------------------------------------------
// openGauss (DBMS_JOB through pkg_service)

const GAUSS_CLEANUP: &str = "SELECT pkg_service.job_cancel(job_id) FROM pg_catalog.pg_job_proc
       WHERE what LIKE '%dbine_test_jobs%';
     DROP TABLE IF EXISTS dbine_test_jobs_log";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss_dbms_job() {
    let Some(cfg) = cfg("opengauss", "DBINE_TEST_OPENGAUSS_URL") else {
        eprintln!("DBINE_TEST_OPENGAUSS_URL not set; skipping");
        return;
    };
    let mut s = connect("opengauss", &cfg).await;
    exec(&mut s, GAUSS_CLEANUP).await;
    let body = async {
        exec(&mut s, "CREATE TABLE dbine_test_jobs_log (t timestamp)").await;
        let submit = |what: &str| {
            format!("SELECT pkg_service.job_submit(NULL, '{what}', sysdate + 1, 'sysdate + 1')")
        };
        let ok = format!(
            "dbms_job:{}",
            scalar(
                &mut s,
                &submit("insert into dbine_test_jobs_log values (sysdate);")
            )
            .await
        );
        let bad = format!(
            "dbms_job:{}",
            scalar(
                &mut s,
                &submit("insert into dbine_test_jobs_missing values (1);")
            )
            .await
        );
        let list = s.jobs().await.unwrap();
        eprintln!("{list:#?}");
        let j = list
            .jobs
            .iter()
            .find(|j| j.id == ok)
            .expect("the job")
            .clone();
        assert_eq!(j.kind, "DBMS_JOB");
        assert_eq!(j.schedule.as_deref(), Some("sysdate + 1"));
        assert_eq!(j.group.as_deref(), Some(cfg.database.as_str()));
        assert!(j.enabled && j.can_run && j.can_enable && j.has_script);
        assert!(!j.can_stop && !j.has_history && j.next_run.is_some());

        let d = s.job_detail(&ok).await.unwrap();
        eprintln!("{d:#?}");
        assert_eq!(
            d.steps[0].command.as_deref(),
            Some("insert into dbine_test_jobs_log values (sysdate);")
        );

        // Run now: the job scheduler takes it within a second or two.
        s.run_job(&ok).await.unwrap();
        s.run_job(&bad).await.unwrap();
        wait_for!(60, "openGauss to run both", {
            let list = s.jobs().await.unwrap().jobs;
            let o = list.iter().find(|j| j.id == ok).unwrap();
            let b = list.iter().find(|j| j.id == bad).unwrap();
            o.last_outcome == Some(JobOutcome::Succeeded)
                && b.last_outcome == Some(JobOutcome::Failed)
        });
        assert_eq!(
            scalar(&mut s, "SELECT count(*) FROM dbine_test_jobs_log").await,
            "1"
        );
        let j = s.job_detail(&ok).await.unwrap().job;
        assert!(
            j.last_started.is_some() && j.last_duration_ms.is_some(),
            "{j:?}"
        );
        let b = s.job_detail(&bad).await.unwrap();
        assert!(
            b.properties.iter().any(|p| p.label == "Último error"),
            "{b:#?}"
        );
        assert!(matches!(
            s.job_runs(&ok, 5).await,
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(s.stop_job(&ok).await, Err(Error::Unsupported(_))));

        // Disable (broken) and enable.
        s.set_job_enabled(&ok, false).await.unwrap();
        let off = s.job_detail(&ok).await.unwrap().job;
        assert!(!off.enabled && !off.can_run && off.next_run.is_none());
        assert!(s.run_job(&ok).await.is_err());
        s.set_job_enabled(&ok, true).await.unwrap();
        assert!(s.job_detail(&ok).await.unwrap().job.enabled);

        // The script submits it again, its next run as it is even when it's
        // read under another DateStyle.
        let next = |id: &str| {
            format!(
                "SELECT to_char(next_run_date, 'YYYY-MM-DD HH24:MI:SS.US') FROM pg_catalog.pg_job WHERE job_id = {id}"
            )
        };
        let before = scalar(&mut s, &next(&ok[9..])).await;
        exec(&mut s, "SET DateStyle = 'SQL, DMY'").await;
        let script = s.job_script(&ok).await.unwrap();
        exec(&mut s, "RESET DateStyle").await;
        eprintln!("{script}");
        assert!(
            script.contains(
                "pkg_service.job_submit(NULL, E'insert into dbine_test_jobs_log values (sysdate);'"
            ),
            "{script}"
        );
        exec(
            &mut s,
            &format!("SELECT pkg_service.job_cancel({})", &ok[9..]),
        )
        .await;
        let again = scalar(&mut s, &script).await;
        let again = format!("dbms_job:{again}");
        assert_ne!(again, ok);
        assert_eq!(
            s.job_detail(&again).await.unwrap().job.schedule.as_deref(),
            Some("sysdate + 1")
        );
        assert_eq!(scalar(&mut s, &next(&again[9..])).await, before);

        refuses_bad_ids(&mut s, &bad).await;
        assert!(s.job_detail(&bad).await.is_ok(), "the job is still there");
    };
    guarded(body, async {
        let mut c = connect("opengauss", &cfg).await;
        exec(&mut c, GAUSS_CLEANUP).await;
    })
    .await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT count(*) FROM pg_catalog.pg_job_proc WHERE what LIKE '%dbine_test_jobs%'"
        )
        .await,
        "0"
    );
}

// ---------------------------------------------------------------------------
// pgAgent

const AGENT_CLEANUP: &str =
    "DELETE FROM pgagent.pga_job WHERE jobname LIKE 'dbine\\_test\\_jobs\\_%'";

/// A pgAgent job with one SQL step and a schedule that starts tomorrow,
/// so it only runs when asked.
fn agent_job(name: &str, code: &str) -> String {
    format!(
        "DO $$
         DECLARE jid integer;
         BEGIN
           INSERT INTO pgagent.pga_job (jobjclid, jobname, jobdesc, jobenabled)
           VALUES (1, '{name}', 'dbine test', true) RETURNING jobid INTO jid;
           INSERT INTO pgagent.pga_jobstep (jstjobid, jstname, jstkind, jstcode, jstdbname, jstonerror)
           VALUES (jid, 'paso uno', 's', '{code}', current_database(), 'f');
           INSERT INTO pgagent.pga_schedule (jscjobid, jscname, jscstart)
           VALUES (jid, 'diaria', now() + interval '1 day');
         END $$"
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pgagent() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_PGCRON_URL") else {
        eprintln!("DBINE_TEST_PGCRON_URL not set; skipping");
        return;
    };
    let mut s = connect("postgres", &cfg).await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT count(*) FROM pg_extension WHERE extname = 'pgagent'"
        )
        .await,
        "1",
        "the server needs pgAgent (see the top of this file)"
    );
    let agents: u32 = scalar(&mut s, "SELECT count(*) FROM pgagent.pga_jobagent")
        .await
        .parse()
        .unwrap();
    exec(&mut s, AGENT_CLEANUP).await;
    let body = async {
        exec(&mut s, &agent_job("dbine_test_jobs_agent", "SELECT 1")).await;
        exec(
            &mut s,
            &agent_job("dbine_test_jobs_agent_fail", "SELECT 1/0"),
        )
        .await;
        let list = s.jobs().await.unwrap();
        eprintln!("{list:#?}");
        let j = find(&list.jobs, "dbine_test_jobs_agent").clone();
        let bad = find(&list.jobs, "dbine_test_jobs_agent_fail").id.clone();
        assert!(j.id.starts_with("pgagent:"));
        assert_eq!(j.kind, "pgAgent");
        assert_eq!(j.schedule.as_deref(), Some("* * * * *"));
        assert_eq!(j.description.as_deref(), Some("dbine test"));
        assert!(j.next_run.is_some() && j.enabled && j.can_enable && j.has_history && j.has_script);
        assert!(!j.can_stop);
        assert_eq!(j.can_run, agents > 0, "run now needs an agent");
        if agents == 0 {
            assert!(list.notes.iter().any(|n| n.contains("agente")), "{list:?}");
        }

        // Detail.
        let d = s.job_detail(&j.id).await.unwrap();
        eprintln!("{d:#?}");
        assert_eq!(d.steps.len(), 1);
        assert_eq!(d.steps[0].kind.as_deref(), Some("SQL"));
        assert_eq!(d.steps[0].command.as_deref(), Some("SELECT 1"));
        assert_eq!(d.steps[0].target.as_deref(), Some(cfg.database.as_str()));
        assert_eq!(d.schedules.len(), 1);
        assert!(
            d.schedules[0].starts_with("diaria: * * * * *, desde "),
            "{:?}",
            d.schedules
        );

        // Run now: the agent picks it up.
        if agents > 0 {
            s.run_job(&j.id).await.unwrap();
            s.run_job(&bad).await.unwrap();
            wait_for!(60, "pgAgent to run both", {
                let list = s.jobs().await.unwrap().jobs;
                find(&list, "dbine_test_jobs_agent").last_outcome == Some(JobOutcome::Succeeded)
                    && find(&list, "dbine_test_jobs_agent_fail").last_outcome
                        == Some(JobOutcome::Failed)
            });
            let runs = s.job_runs(&j.id, 10).await.unwrap();
            eprintln!("{runs:#?}");
            assert_eq!(runs[0].outcome, JobOutcome::Succeeded);
            assert_eq!(runs[0].steps.len(), 1);
            assert_eq!(runs[0].steps[0].name, "paso uno");
            assert!(runs[0].started.is_some() && runs[0].duration_ms.is_some());
            let failed = s.job_runs(&bad, 10).await.unwrap();
            assert_eq!(failed[0].outcome, JobOutcome::Failed);
            assert!(
                failed[0]
                    .message
                    .as_deref()
                    .unwrap_or("")
                    .contains("division by zero"),
                "{failed:#?}"
            );
        }

        // Disable: no run now; enable again.
        s.set_job_enabled(&j.id, false).await.unwrap();
        let off = job(&mut s, "dbine_test_jobs_agent").await;
        assert!(!off.enabled && !off.can_run && off.next_run.is_none());
        assert!(s.run_job(&j.id).await.is_err());
        s.set_job_enabled(&j.id, true).await.unwrap();
        assert!(job(&mut s, "dbine_test_jobs_agent").await.enabled);
        assert!(matches!(
            s.stop_job(&j.id).await,
            Err(Error::Unsupported(_))
        ));

        // The script creates it again, with its dates as they are even when
        // it's read under another DateStyle (an exception day, 5 December,
        // would come back as 12 May).
        let jid = j.id[8..].to_string();
        exec(
            &mut s,
            &format!(
                "INSERT INTO pgagent.pga_exception (jexscid, jexdate, jextime)
                 SELECT jscid, DATE '2026-12-05', NULL FROM pgagent.pga_schedule WHERE jscjobid = {jid}"
            ),
        )
        .await;
        let dates = |id: &str| {
            format!(
                "SELECT extract(epoch FROM s.jscstart)::text || ' ' || e.jexdate::text
                 FROM pgagent.pga_schedule s JOIN pgagent.pga_exception e ON e.jexscid = s.jscid
                 WHERE s.jscjobid = {id}"
            )
        };
        let before = scalar(&mut s, &dates(&jid)).await;
        assert!(before.ends_with(" 2026-12-05"), "{before}");
        exec(&mut s, "SET DateStyle = 'SQL, DMY'").await;
        let script = s.job_script(&j.id).await.unwrap();
        exec(&mut s, "RESET DateStyle").await;
        eprintln!("{script}");
        assert!(
            script.contains("INSERT INTO pgagent.pga_jobstep"),
            "{script}"
        );
        exec(
            &mut s,
            &format!("DELETE FROM pgagent.pga_job WHERE jobid = {}", &j.id[8..]),
        )
        .await;
        exec(&mut s, &script).await;
        let again = job(&mut s, "dbine_test_jobs_agent").await;
        assert_ne!(again.id, j.id);
        assert_eq!(again.schedule.as_deref(), Some("* * * * *"));
        let d = s.job_detail(&again.id).await.unwrap();
        assert_eq!(d.steps[0].command.as_deref(), Some("SELECT 1"));
        assert_eq!(d.schedules.len(), 1);
        assert_eq!(scalar(&mut s, &dates(&again.id[8..])).await, before);

        refuses_bad_ids(&mut s, &bad).await;
        assert!(s.job_detail(&bad).await.is_ok(), "the job is still there");
    };
    guarded(body, async {
        let mut c = connect("postgres", &cfg).await;
        exec(&mut c, AGENT_CLEANUP).await;
    })
    .await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT count(*) FROM pgagent.pga_job WHERE jobname LIKE 'dbine\\_test\\_jobs\\_%'"
        )
        .await,
        "0"
    );
}

// ---------------------------------------------------------------------------
// TimescaleDB

/// Turns `timescaledb.enable_job_execution_logging` back to its default
/// when the test turned it on.
async fn ts_logging_off(s: &mut Box<dyn Session>, turned_on: bool) {
    if turned_on {
        exec(
            s,
            "ALTER SYSTEM RESET timescaledb.enable_job_execution_logging",
        )
        .await;
        exec(s, "SELECT pg_reload_conf()").await;
    }
}

const TS_CLEANUP: &str = "SELECT count(delete_job(job_id)) FROM timescaledb_information.jobs
       WHERE proc_name = 'dbine_test_jobs_proc' OR hypertable_name = 'dbine_test_jobs_ht';
     DROP TABLE IF EXISTS public.dbine_test_jobs_ht;
     DROP PROCEDURE IF EXISTS public.dbine_test_jobs_proc(int, jsonb);
     DO $$ BEGIN
       IF to_regclass('_timescaledb_internal.bgw_job_stat_history') IS NOT NULL THEN
         DELETE FROM _timescaledb_internal.bgw_job_stat_history
          WHERE data -> 'job' ->> 'proc_name' = 'dbine_test_jobs_proc';
       END IF;
     END $$";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescaledb() {
    let Some(cfg) = cfg("timescaledb", "DBINE_TEST_TIMESCALEDB_URL") else {
        eprintln!("DBINE_TEST_TIMESCALEDB_URL not set; skipping");
        return;
    };
    let mut s = connect("timescaledb", &cfg).await;
    exec(&mut s, "CREATE EXTENSION IF NOT EXISTS timescaledb").await;
    exec(&mut s, TS_CLEANUP).await;
    // Logging every run (else only failures are), when it's at its default.
    let logging = scalar(
        &mut s,
        "SELECT setting || ' ' || source FROM pg_settings WHERE name = 'timescaledb.enable_job_execution_logging'",
    )
    .await;
    let turn_on = logging == "off default";
    let logged = turn_on || logging.starts_with("on ");
    let body = async {
        if turn_on {
            exec(
                &mut s,
                "ALTER SYSTEM SET timescaledb.enable_job_execution_logging = on",
            )
            .await;
            exec(&mut s, "SELECT pg_reload_conf()").await;
        }
        exec(
            &mut s,
            "CREATE PROCEDURE public.dbine_test_jobs_proc(job_id int, config jsonb) LANGUAGE plpgsql AS $$
             BEGIN
               IF (config ->> 'fail')::boolean THEN RAISE EXCEPTION 'dbine_test_jobs boom'; END IF;
               PERFORM pg_sleep(coalesce((config ->> 'sleep')::float, 0.2));
             END $$",
        )
        .await;
        let add = |fail: bool, name: &str| {
            format!(
                "SELECT add_job('public.dbine_test_jobs_proc', '1 day', config => '{{\"fail\": {fail}}}',
                                initial_start => now() + interval '1 day', job_name => '{name}')"
            )
        };
        let ok = format!(
            "timescaledb:{}",
            scalar(&mut s, &add(false, "dbine_test_jobs_ok")).await
        );
        let bad = format!(
            "timescaledb:{}",
            scalar(&mut s, &add(true, "dbine_test_jobs_fail")).await
        );
        let slow = format!(
            "timescaledb:{}",
            scalar(
                &mut s,
                "SELECT add_job('public.dbine_test_jobs_proc', '1 day', config => '{\"fail\": false, \"sleep\": 8}',
                                initial_start => now() + interval '1 day', job_name => 'dbine_test_jobs_slow')"
            )
            .await
        );
        exec(
            &mut s,
            "CREATE TABLE public.dbine_test_jobs_ht (t timestamptz NOT NULL, v int);
             SELECT create_hypertable('public.dbine_test_jobs_ht', 't');
             SELECT add_retention_policy('public.dbine_test_jobs_ht', drop_after => interval '30 days')",
        )
        .await;

        let list = s.jobs().await.unwrap();
        eprintln!("{list:#?}");
        let j = find(&list.jobs, "dbine_test_jobs_ok").clone();
        assert_eq!(j.id, ok);
        assert_eq!(j.kind, "TimescaleDB");
        assert_eq!(j.group.as_deref(), Some("public"));
        assert_eq!(j.schedule.as_deref(), Some("INTERVAL '1 day'"));
        assert!(
            j.enabled && j.can_run && j.can_enable && j.has_history && j.has_script && !j.can_stop
        );
        assert!(j.next_run.is_some());
        let policy = list
            .jobs
            .iter()
            .find(|p| p.group.as_deref() == Some("public.dbine_test_jobs_ht"))
            .expect("the retention policy");
        assert_eq!(policy.kind, "TimescaleDB · retención");
        assert!(list
            .jobs
            .iter()
            .any(|p| p.kind == "TimescaleDB · sistema" && !p.has_script));

        // Run now returns at once; the scheduler runs it.
        let asked = Instant::now();
        s.run_job(&ok).await.unwrap();
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "run_job doesn't wait for the run"
        );
        s.run_job(&bad).await.unwrap();
        wait_for!(60, "TimescaleDB to run both", {
            let list = s.jobs().await.unwrap().jobs;
            find(&list, "dbine_test_jobs_ok").last_outcome == Some(JobOutcome::Succeeded)
                && find(&list, "dbine_test_jobs_fail").last_outcome == Some(JobOutcome::Failed)
        });
        let runs = s.job_runs(&ok, 10).await.unwrap();
        eprintln!("{runs:#?}");
        assert_eq!(runs[0].outcome, JobOutcome::Succeeded);
        assert!(runs[0].started.is_some());
        let failed = s.job_runs(&bad, 10).await.unwrap();
        eprintln!("{failed:#?}");
        assert_eq!(failed[0].outcome, JobOutcome::Failed);
        assert!(
            failed[0]
                .message
                .as_deref()
                .unwrap_or("")
                .contains("dbine_test_jobs boom"),
            "{failed:#?}"
        );

        // With every run logged, the one going on is logged at its start
        // without an outcome: it's running, not failed.
        if logged {
            s.run_job(&slow).await.unwrap();
            wait_for!(60, "the slow job to start", {
                s.job_runs(&slow, 5)
                    .await
                    .unwrap()
                    .first()
                    .is_some_and(|r| r.outcome == JobOutcome::Running)
            });
            let runs = s.job_runs(&slow, 5).await.unwrap();
            eprintln!("{runs:#?}");
            assert_eq!(runs.len(), 1, "logged once: {runs:#?}");
            assert!(runs[0].duration_ms.is_none() && runs[0].message.is_none());
            assert!(job(&mut s, "dbine_test_jobs_slow").await.running);
            wait_for!(60, "the slow job to end", {
                s.job_runs(&slow, 5).await.unwrap()[0].outcome == JobOutcome::Succeeded
            });
            assert_eq!(s.job_runs(&slow, 5).await.unwrap().len(), 1);
        } else {
            eprintln!("timescaledb.enable_job_execution_logging is {logging}: not changed, the run-in-progress check skipped");
        }

        // Detail.
        let d = s.job_detail(&ok).await.unwrap();
        eprintln!("{d:#?}");
        assert_eq!(d.steps[0].kind.as_deref(), Some("PL/pgSQL"));
        assert!(d.steps[0]
            .command
            .as_deref()
            .unwrap()
            .starts_with("CALL \"public\".\"dbine_test_jobs_proc\"("));
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Intervalo" && p.value == "1 day"));

        // Disable and enable.
        s.set_job_enabled(&ok, false).await.unwrap();
        let off = job(&mut s, "dbine_test_jobs_ok").await;
        assert!(!off.enabled && !off.can_run);
        assert!(s.run_job(&ok).await.is_err());
        s.set_job_enabled(&ok, true).await.unwrap();
        assert!(job(&mut s, "dbine_test_jobs_ok").await.enabled);
        assert!(matches!(s.stop_job(&ok).await, Err(Error::Unsupported(_))));

        // Scripts: add_job for the action, the policy's own call.
        let script = s.job_script(&ok).await.unwrap();
        eprintln!("{script}");
        assert!(
            script.contains("add_job(E'\"public\".\"dbine_test_jobs_proc\"', INTERVAL E'1 day'"),
            "{script}"
        );
        exec(&mut s, &format!("SELECT delete_job({})", &ok[12..])).await;
        exec(&mut s, &script).await;
        let again = job(&mut s, "dbine_test_jobs_ok").await;
        assert_ne!(again.id, ok);
        let policy_script = s.job_script(&policy.id).await.unwrap();
        eprintln!("{policy_script}");
        assert!(
            policy_script.contains("add_retention_policy(E'\"public\".\"dbine_test_jobs_ht\"', drop_after => INTERVAL E'30 days'"),
            "{policy_script}"
        );
        exec(
            &mut s,
            "SELECT remove_retention_policy('public.dbine_test_jobs_ht')",
        )
        .await;
        exec(&mut s, &policy_script).await;
        let list = s.jobs().await.unwrap().jobs;
        assert!(list.iter().any(|p| p.kind == "TimescaleDB · retención"
            && p.group.as_deref() == Some("public.dbine_test_jobs_ht")));
        let system = list
            .iter()
            .find(|p| p.kind == "TimescaleDB · sistema")
            .unwrap();
        assert!(matches!(
            s.job_script(&system.id).await,
            Err(Error::Unsupported(_))
        ));

        refuses_bad_ids(&mut s, &bad).await;
        assert!(s.job_detail(&bad).await.is_ok(), "the job is still there");
    };
    guarded(body, async {
        let mut c = connect("timescaledb", &cfg).await;
        exec(&mut c, TS_CLEANUP).await;
        ts_logging_off(&mut c, turn_on).await;
    })
    .await;
    // The reload reaches this session at its next statements.
    wait_for!(10, "the logging setting as it was", {
        scalar(
            &mut s,
            "SELECT setting || ' ' || source FROM pg_settings WHERE name = 'timescaledb.enable_job_execution_logging'",
        )
        .await
            == logging
    });
    assert_eq!(
        scalar(&mut s, "SELECT count(*) FROM timescaledb_information.jobs WHERE proc_name = 'dbine_test_jobs_proc'").await,
        "0"
    );
}

// ---------------------------------------------------------------------------
// CockroachDB

/// The cluster settings the test changes, as they were.
struct CrdbSettings {
    rangefeed: &'static str,
    /// `jobs.scheduler.pace`, and whether it was at its default.
    pace: String,
    pace_default: bool,
}

async fn crdb_settings(s: &mut Box<dyn Session>) -> CrdbSettings {
    let rangefeed = scalar(s, "SHOW CLUSTER SETTING kv.rangefeed.enabled").await;
    let pace = |col: &str| {
        format!(
            "SELECT {col} FROM [SHOW ALL CLUSTER SETTINGS] WHERE variable = 'jobs.scheduler.pace'"
        )
    };
    CrdbSettings {
        rangefeed: if rangefeed == "true" { "true" } else { "false" },
        pace: scalar(s, &pace("value")).await,
        pace_default: scalar(s, &pace("origin")).await == "default",
    }
}

/// The test's backups (cancelled if still going), schedules, changefeeds,
/// table and backup files, and the cluster settings as they were.
async fn crdb_cleanup(s: &mut Box<dyn Session>, settings: &CrdbSettings) {
    let _ = run(
        s,
        "CANCEL JOBS SELECT job_id FROM [SHOW JOBS] WHERE job_type = 'BACKUP'
           AND description LIKE '%dbine_test_jobs_backup%' AND status IN ('running', 'pending', 'paused')",
    )
    .await;
    exec(
        s,
        "DROP SCHEDULES SELECT id FROM [SHOW SCHEDULES] WHERE label LIKE 'dbine_test_jobs%'",
    )
    .await;
    // A changefeed on its way to pause can't be cancelled until it's paused.
    let ours = "FROM [SHOW CHANGEFEED JOBS] WHERE description LIKE '%dbine_test_jobs_t%'";
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let _ = run(
            s,
            &format!(
                "CANCEL JOBS SELECT job_id {ours} AND status IN ('running', 'paused', 'pending')"
            ),
        )
        .await;
        let left = scalar(
            s,
            &format!("SELECT count(*) {ours} AND status IN ('running', 'paused', 'pending', 'pause-requested', 'resume-requested')"),
        )
        .await;
        if left == "0" || Instant::now() > until {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    exec(s, "DROP TABLE IF EXISTS defaultdb.dbine_test_jobs_t").await;
    // The backup's files (userfile:// keeps them in two tables of the
    // user's); the tables go too when nothing else is left in them.
    let user = scalar(s, "SELECT current_user").await;
    let files = format!("defaultdb.public.userfiles_{user}_upload_files");
    let payload = format!("defaultdb.public.userfiles_{user}_upload_payload");
    let there = scalar(
        s,
        &format!("SELECT count(*) FROM [SHOW TABLES FROM defaultdb.public] WHERE table_name = 'userfiles_{user}_upload_files'"),
    )
    .await;
    if there == "1" {
        let ours = "filename LIKE '/dbine\\_test\\_jobs\\_backup/%'";
        exec(
            s,
            &format!(
                "DELETE FROM {payload} WHERE file_id IN (SELECT file_id FROM {files} WHERE {ours})"
            ),
        )
        .await;
        exec(s, &format!("DELETE FROM {files} WHERE {ours}")).await;
        if scalar(s, &format!("SELECT count(*) FROM {files}")).await == "0" {
            exec(s, &format!("DROP TABLE {payload}")).await;
            exec(s, &format!("DROP TABLE {files}")).await;
        }
    }
    exec(
        s,
        &format!(
            "SET CLUSTER SETTING kv.rangefeed.enabled = {}",
            settings.rangefeed
        ),
    )
    .await;
    if settings.pace_default {
        exec(s, "RESET CLUSTER SETTING jobs.scheduler.pace").await;
    } else {
        exec(
            s,
            &format!(
                "SET CLUSTER SETTING jobs.scheduler.pace = '{}'",
                settings.pace
            ),
        )
        .await;
    }
}

/// Waits for schedule `id`'s first run to end and returns its runs.
async fn crdb_ran(s: &mut Box<dyn Session>, id: &str) -> Vec<dbine_driver::JobRun> {
    wait_for!(180, format!("{id} to start"), {
        !s.job_runs(id, 5).await.unwrap().is_empty()
    });
    wait_for!(120, format!("{id} to end"), {
        s.job_runs(id, 5).await.unwrap()[0].outcome != JobOutcome::Running
    });
    s.job_runs(id, 5).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroachdb() {
    let Some(cfg) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let mut s = connect("cockroachdb", &cfg).await;
    let settings = crdb_settings(&mut s).await;
    let banner = scalar(&mut s, "SELECT version()").await;
    // EXECUTE SCHEDULE (any schedule but backups) is 26.1+.
    let major: u32 = banner
        .split(" v")
        .nth(1)
        .and_then(|v| v.split('.').next())
        .and_then(|m| m.parse().ok())
        .unwrap_or(0);
    crdb_cleanup(&mut s, &settings).await;
    let body = async {
        exec(&mut s, "SET CLUSTER SETTING kv.rangefeed.enabled = true").await;
        // The scheduler's pass every 5 s instead of every minute.
        exec(&mut s, "SET CLUSTER SETTING jobs.scheduler.pace = '5s'").await;
        exec(
            &mut s,
            "CREATE TABLE defaultdb.dbine_test_jobs_t (id INT PRIMARY KEY)",
        )
        .await;
        // A full backup every day and an incremental one every hour: two
        // schedules with one label.
        exec(
            &mut s,
            "CREATE SCHEDULE dbine_test_jobs_backup FOR BACKUP TABLE defaultdb.dbine_test_jobs_t
             INTO 'userfile:///dbine_test_jobs_backup' RECURRING '@hourly' FULL BACKUP '@daily'",
        )
        .await;
        exec(
            &mut s,
            "CREATE SCHEDULE dbine_test_jobs_cf FOR CHANGEFEED TABLE defaultdb.dbine_test_jobs_t
             INTO 'null://' WITH initial_scan = 'only' RECURRING '@daily'",
        )
        .await;
        let feed = scalar(
            &mut s,
            "CREATE CHANGEFEED FOR TABLE defaultdb.dbine_test_jobs_t INTO 'null://'",
        )
        .await;
        let feed = format!("crdb_job:{feed}");

        let list = s.jobs().await.unwrap();
        eprintln!("{list:#?}");
        let (inc, full): (Vec<ServerJob>, Vec<ServerJob>) = list
            .jobs
            .iter()
            .filter(|j| j.name == "dbine_test_jobs_backup")
            .cloned()
            .partition(|j| {
                j.description
                    .as_deref()
                    .is_some_and(|d| d.contains("INTO LATEST IN"))
            });
        assert_eq!((inc.len(), full.len()), (1, 1), "{list:#?}");
        let (inc, b) = (inc[0].clone(), full[0].clone());
        assert!(b.id.starts_with("crdb_schedule:"));
        assert_eq!(b.kind, "Schedule · backup");
        assert_eq!(b.schedule.as_deref(), Some("@daily"));
        assert!(b.enabled && b.can_run && b.can_enable && b.has_history && b.has_script);
        assert!(b.next_run.is_some());
        // The incremental one waits, paused, for the first full backup.
        assert_eq!(inc.schedule.as_deref(), Some("@hourly"));
        assert!(!inc.enabled && !inc.can_run);
        let cfs = find(&list.jobs, "dbine_test_jobs_cf").clone();
        assert_eq!(cfs.kind, "Schedule · changefeed");
        assert_eq!(cfs.can_run, major >= 26, "EXECUTE SCHEDULE is 26.1+");
        assert!(cfs.has_script && cfs.can_enable);
        let c = list
            .jobs
            .iter()
            .find(|j| j.id == feed)
            .expect("the changefeed")
            .clone();
        assert_eq!(c.kind, "Changefeed");
        assert!(c.name.contains("dbine_test_jobs_t"));
        assert!(
            c.enabled && c.can_stop && c.can_enable && c.has_script && !c.has_history && !c.can_run
        );
        assert!(list
            .jobs
            .iter()
            .any(|j| j.kind == "Schedule · sistema" && j.can_run == (major >= 26)));

        // Detail and scripts.
        let d = s.job_detail(&b.id).await.unwrap();
        eprintln!("{d:#?}");
        assert!(d.steps[0]
            .command
            .as_deref()
            .unwrap()
            .starts_with("BACKUP TABLE"));
        let script = s.job_script(&b.id).await.unwrap();
        eprintln!("{script}");
        assert!(
            script.contains("CREATE SCHEDULE 'dbine_test_jobs_backup' FOR BACKUP"),
            "{script}"
        );
        let cf_script = s.job_script(&cfs.id).await.unwrap();
        eprintln!("{cf_script}");
        assert!(cf_script.contains("FOR CHANGEFEED"), "{cf_script}");
        let cs = s.job_script(&feed).await.unwrap();
        assert!(cs.contains("CREATE CHANGEFEED FOR TABLE"), "{cs}");
        let cd = s.job_detail(&feed).await.unwrap();
        assert!(cd.properties.iter().any(|p| p.label == "Estado"));
        assert!(matches!(
            s.job_runs(&feed, 5).await,
            Err(Error::Unsupported(_))
        ));

        // Pause and resume the schedule and the changefeed.
        s.set_job_enabled(&b.id, false).await.unwrap();
        let paused = s.job_detail(&b.id).await.unwrap().job;
        assert!(!paused.enabled && !paused.can_run);
        assert!(
            s.run_job(&b.id).await.is_err(),
            "a paused schedule doesn't run now"
        );
        s.set_job_enabled(&b.id, true).await.unwrap();
        assert!(s.job_detail(&b.id).await.unwrap().job.enabled);
        s.set_job_enabled(&feed, false).await.unwrap();
        wait_for!(30, "the changefeed to pause", {
            let d = s.job_detail(&feed).await.unwrap();
            // Paused, not just asked to: then it can be resumed.
            !d.job.enabled && d.job.can_enable
        });
        s.set_job_enabled(&feed, true).await.unwrap();
        wait_for!(30, "the changefeed to resume", {
            let d = s.job_detail(&feed).await.unwrap();
            d.job.enabled && d.job.running
        });

        // Run the full backup now: with FULL, since the plain form runs the
        // pair's incremental schedule (still paused here, so it'd fail).
        let asked = Instant::now();
        s.run_job(&b.id).await.unwrap();
        assert!(
            asked.elapsed() < Duration::from_secs(10),
            "run_job doesn't wait for the run"
        );
        let runs = crdb_ran(&mut s, &b.id).await;
        eprintln!("{runs:#?}");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, JobOutcome::Succeeded);
        assert!(runs[0].started.is_some() && runs[0].duration_ms.is_some());
        assert!(
            s.job_runs(&inc.id, 5).await.unwrap().is_empty(),
            "the incremental schedule didn't run"
        );
        let b = s.job_detail(&b.id).await.unwrap().job;
        assert_eq!(b.last_outcome, Some(JobOutcome::Succeeded));
        assert!(
            matches!(s.stop_job(&b.id).await, Err(Error::Query(_))),
            "nothing running to stop"
        );

        // With the full backup done, the incremental schedule is resumed
        // and runs now too, without running the full one again.
        wait_for!(30, "the incremental schedule to resume", {
            let j = s.job_detail(&inc.id).await.unwrap().job;
            j.enabled && j.can_run
        });
        s.run_job(&inc.id).await.unwrap();
        let runs = crdb_ran(&mut s, &inc.id).await;
        assert_eq!(runs[0].outcome, JobOutcome::Succeeded, "{runs:#?}");
        assert_eq!(s.job_runs(&b.id, 5).await.unwrap().len(), 1);

        // Any other schedule runs now with EXECUTE SCHEDULE (26.1+).
        if major >= 26 {
            s.run_job(&cfs.id).await.unwrap();
            let runs = crdb_ran(&mut s, &cfs.id).await;
            eprintln!("{runs:#?}");
            assert_eq!(runs[0].outcome, JobOutcome::Succeeded);
        } else {
            assert!(matches!(
                s.run_job(&cfs.id).await,
                Err(Error::Unsupported(_))
            ));
        }

        // Stopping the changefeed ends it: it leaves the list once
        // cancelled, and can't be stopped again while it's being cancelled.
        s.stop_job(&feed).await.unwrap();
        wait_for!(60, "the changefeed to end", {
            match s.jobs().await.unwrap().jobs.iter().find(|j| j.id == feed) {
                None => true,
                Some(j) => !j.can_stop && !j.can_enable,
            }
        });

        refuses_bad_ids(&mut s, &b.id).await;
        assert!(
            s.job_detail("pg_cron:1").await.is_err(),
            "another engine's id"
        );
        assert!(
            s.job_detail(&b.id).await.is_ok(),
            "the schedule is still there"
        );
    };
    guarded(body, async {
        let mut c = connect("cockroachdb", &cfg).await;
        crdb_cleanup(&mut c, &settings).await;
    })
    .await;
    assert_eq!(
        scalar(
            &mut s,
            "SELECT count(*) FROM [SHOW SCHEDULES] WHERE label LIKE 'dbine_test_jobs%'"
        )
        .await,
        "0"
    );
    let after = crdb_settings(&mut s).await;
    assert_eq!(
        (after.rangefeed, after.pace, after.pace_default),
        (
            settings.rangefeed,
            settings.pace.clone(),
            settings.pace_default
        ),
        "the cluster settings are as they were"
    );
}

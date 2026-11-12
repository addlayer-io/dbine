//! Server jobs (DBMS_SCHEDULER) against a real server: list, detail, run
//! now, history (with a chain's steps), stop, disable / enable, the script,
//! legacy DBMS_JOB jobs and ids that aren't what the driver produced. The
//! jobs are created in the admin's schema (SYSTEM on the test container),
//! and a user with only CREATE SESSION and CREATE JOB checks the partial
//! list (then CREATE ANY JOB). Last, job_queue_processes goes to 0 for a
//! moment and back to its value. Everything the test makes starts with
//! DBINE_TEST_JOBS_ and is dropped at the end, also when an assertion fails:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test jobs -- --ignored
//! ```

use dbine_driver::jobs::{JobOutcome, JobRun, ServerJob};
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::{Duration, Instant};

const USER: &str = "DBINE_TEST_JOBS_USER";
const USER_PASSWORD: &str = "DbineJobs_2026";

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL").ok()?;
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.rsplit_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    Some(cfg)
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 100, &mut out).await {
        panic!("{sql}\n→ {e}");
    }
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap();
    let v = &out.results.last().unwrap().rows[0][0];
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

/// Drops what the test made (by its prefix only), whatever it was left in.
const CLEANUP: &str = r#"BEGIN
  FOR j IN (SELECT owner, job_name FROM dba_scheduler_jobs
             WHERE job_name LIKE 'DBINE\_TEST\_JOBS\_%' ESCAPE '\') LOOP
    BEGIN
      DBMS_SCHEDULER.DROP_JOB('"' || j.owner || '"."' || j.job_name || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL;
    END;
  END LOOP;
  FOR c IN (SELECT owner, chain_name FROM dba_scheduler_chains
             WHERE chain_name LIKE 'DBINE\_TEST\_JOBS\_%' ESCAPE '\') LOOP
    BEGIN
      DBMS_SCHEDULER.DROP_CHAIN('"' || c.owner || '"."' || c.chain_name || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL;
    END;
  END LOOP;
  FOR p IN (SELECT owner, program_name FROM dba_scheduler_programs
             WHERE program_name LIKE 'DBINE\_TEST\_JOBS\_%' ESCAPE '\') LOOP
    BEGIN
      DBMS_SCHEDULER.DROP_PROGRAM('"' || p.owner || '"."' || p.program_name || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL;
    END;
  END LOOP;
  FOR j IN (SELECT job FROM user_jobs WHERE what LIKE '%dbine_test_jobs_%') LOOP
    DBMS_JOB.REMOVE(j.job);
  END LOOP;
  COMMIT;
  FOR s IN (SELECT sid, serial# AS serial FROM v$session WHERE username = 'DBINE_TEST_JOBS_USER') LOOP
    BEGIN
      EXECUTE IMMEDIATE 'ALTER SYSTEM KILL SESSION ''' || s.sid || ',' || s.serial || ''' IMMEDIATE';
    EXCEPTION WHEN OTHERS THEN NULL;
    END;
  END LOOP;
  FOR u IN (SELECT username FROM dba_users WHERE username = 'DBINE_TEST_JOBS_USER') LOOP
    EXECUTE IMMEDIATE 'DROP USER DBINE_TEST_JOBS_USER CASCADE';
  END LOOP;
  -- The log outlives a dropped job: a job made again with its name would
  -- show the old runs.
  FOR j IN (SELECT DISTINCT owner, job_name FROM dba_scheduler_job_log
             WHERE job_name LIKE 'DBINE\_TEST\_JOBS\_%' ESCAPE '\') LOOP
    DBMS_SCHEDULER.PURGE_LOG(log_history => 0, which_log => 'JOB_LOG',
      job_name => '"' || j.owner || '"."' || j.job_name || '"');
  END LOOP;
END;"#;

const CREATE: &str = r#"BEGIN
  DBMS_SCHEDULER.CREATE_JOB('DBINE_TEST_JOBS_OK', job_type => 'PLSQL_BLOCK',
    job_action => 'BEGIN DBMS_OUTPUT.PUT_LINE(''dbine ok''); END;',
    repeat_interval => 'FREQ=DAILY;BYHOUR=3', enabled => TRUE, auto_drop => FALSE,
    comments => 'dbine test');
  DBMS_SCHEDULER.CREATE_JOB('DBINE_TEST_JOBS_FAIL', job_type => 'PLSQL_BLOCK',
    job_action => 'BEGIN RAISE_APPLICATION_ERROR(-20001, ''dbine boom''); END;',
    enabled => FALSE, auto_drop => FALSE);
  DBMS_SCHEDULER.CREATE_JOB('DBINE_TEST_JOBS_SLOW', job_type => 'PLSQL_BLOCK',
    job_action => 'BEGIN DBMS_SESSION.SLEEP(60); END;', enabled => FALSE, auto_drop => FALSE);
  DBMS_SCHEDULER.CREATE_PROGRAM('DBINE_TEST_JOBS_P1', 'PLSQL_BLOCK', 'BEGIN NULL; END;',
    enabled => TRUE);
  DBMS_SCHEDULER.CREATE_PROGRAM('DBINE_TEST_JOBS_P2', 'PLSQL_BLOCK',
    'BEGIN RAISE_APPLICATION_ERROR(-20002, ''step boom''); END;', enabled => TRUE);
  DBMS_SCHEDULER.CREATE_CHAIN('DBINE_TEST_JOBS_CH');
  DBMS_SCHEDULER.DEFINE_CHAIN_STEP('DBINE_TEST_JOBS_CH', 'S2', 'DBINE_TEST_JOBS_P2');
  DBMS_SCHEDULER.DEFINE_CHAIN_STEP('DBINE_TEST_JOBS_CH', 'S1', 'DBINE_TEST_JOBS_P1');
  DBMS_SCHEDULER.DEFINE_CHAIN_RULE('DBINE_TEST_JOBS_CH', 'TRUE', 'START S1');
  DBMS_SCHEDULER.DEFINE_CHAIN_RULE('DBINE_TEST_JOBS_CH', 'S1 COMPLETED', 'START S2');
  DBMS_SCHEDULER.DEFINE_CHAIN_RULE('DBINE_TEST_JOBS_CH', 'S2 COMPLETED', 'END');
  DBMS_SCHEDULER.ENABLE('DBINE_TEST_JOBS_CH');
  DBMS_SCHEDULER.CREATE_JOB('DBINE_TEST_JOBS_CHAIN', job_type => 'CHAIN',
    job_action => 'DBINE_TEST_JOBS_CH', enabled => FALSE, auto_drop => FALSE);
  DBMS_SCHEDULER.CREATE_JOB('DBINE_TEST_JOBS_WIN', job_type => 'PLSQL_BLOCK',
    job_action => 'BEGIN NULL; END;', schedule_name => 'SYS.WEEKNIGHT_WINDOW',
    enabled => FALSE, auto_drop => FALSE);
  DBMS_SCHEDULER.CREATE_JOB('DBINE_TEST_JOBS_LW', program_name => 'DBINE_TEST_JOBS_P1',
    repeat_interval => 'FREQ=DAILY;BYHOUR=4', job_style => 'LIGHTWEIGHT',
    enabled => FALSE);
END;"#;

const QUEUE: &str = "SELECT value FROM v$parameter WHERE name = 'job_queue_processes'";

fn find<'a>(jobs: &'a [ServerJob], id: &str) -> &'a ServerJob {
    jobs.iter()
        .find(|j| j.id == id)
        .unwrap_or_else(|| panic!("{id} is listed"))
}

async fn job(s: &mut Box<dyn Session>, id: &str) -> ServerJob {
    find(&s.jobs().await.unwrap().jobs, id).clone()
}

fn rfc3339(t: &Option<String>) -> bool {
    t.as_deref().is_some_and(|t| {
        t.len() == 20 && t.ends_with('Z') && t.as_bytes()[10] == b'T' && t.starts_with("20")
    })
}

/// The newest finished run, once there is one.
async fn finished(s: &mut Box<dyn Session>, id: &str) -> JobRun {
    let until = Instant::now() + Duration::from_secs(90);
    loop {
        let runs = s.job_runs(id, 20).await.unwrap();
        if let Some(r) = runs.into_iter().find(|r| r.outcome != JobOutcome::Running) {
            return r;
        }
        assert!(Instant::now() < until, "{id} never finished a run");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_running(s: &mut Box<dyn Session>, id: &str) -> ServerJob {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let j = job(s, id).await;
        if j.running {
            return j;
        }
        assert!(Instant::now() < until, "{id} never started");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn server_jobs() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    assert!(dbine_driver_oracle::drivers()
        .iter()
        .all(|d| d.capabilities().jobs));
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    exec(&mut admin, CLEANUP).await;
    // The scenario sets it to 0 for a moment: put back as it was.
    let queue = scalar(&mut admin, QUEUE).await;
    assert!(queue.bytes().all(|b| b.is_ascii_digit()), "{queue}");
    // In its own task: whatever fails in it, the cleanup below still runs.
    let result = tokio::spawn(scenario(cfg)).await;
    if scalar(&mut admin, QUEUE).await != queue {
        exec(
            &mut admin,
            &format!("ALTER SYSTEM SET job_queue_processes = {queue} SCOPE = MEMORY"),
        )
        .await;
    }
    exec(&mut admin, CLEANUP).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn scenario(cfg: ConnectionConfig) {
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let me = scalar(&mut s, "SELECT USER FROM dual").await;
    let id = |name: &str| format!("{me}.{name}");
    exec(&mut s, CREATE).await;

    // The list.
    let list = s.jobs().await.unwrap();
    assert!(
        !list.notes.iter().any(|n| n.contains("DBA_SCHEDULER_JOBS")),
        "{:?}",
        list.notes
    );
    let ok = find(&list.jobs, &id("DBINE_TEST_JOBS_OK"));
    eprintln!("{ok:#?}");
    assert_eq!(ok.kind, "DBMS_SCHEDULER");
    assert_eq!(ok.group.as_deref(), Some(me.as_str()));
    assert_eq!(ok.description.as_deref(), Some("dbine test"));
    assert_eq!(ok.schedule.as_deref(), Some("FREQ=DAILY;BYHOUR=3"));
    assert!(ok.enabled && !ok.running && rfc3339(&ok.next_run), "{ok:?}");
    assert!(ok.can_run && ok.can_enable && !ok.can_stop && ok.has_history && ok.has_script);
    assert_eq!(ok.last_outcome, None);
    let fail = find(&list.jobs, &id("DBINE_TEST_JOBS_FAIL"));
    assert!(!fail.enabled && fail.schedule.is_none() && fail.next_run.is_none());
    assert!(fail.can_run, "a disabled job runs on demand");
    let sys = list
        .jobs
        .iter()
        .find(|j| j.id == "SYS.PURGE_LOG")
        .expect("Oracle's own jobs are listed");
    assert!(
        !sys.can_run && !sys.can_enable,
        "SYS's jobs aren't SYSTEM's to manage"
    );
    assert!(sys.schedule.is_some(), "its named schedule: {sys:?}");
    assert!(sys.system, "Oracle's own job: {sys:?}");
    assert!(
        !ok.system && !fail.system,
        "the user's jobs aren't system ones"
    );
    let win = find(&list.jobs, &id("DBINE_TEST_JOBS_WIN"));
    assert_eq!(
        win.schedule.as_deref(),
        Some("Ventana SYS.WEEKNIGHT_WINDOW"),
        "{win:?}"
    );

    // The detail.
    let detail = s.job_detail(&id("DBINE_TEST_JOBS_OK")).await.unwrap();
    assert_eq!(detail.job.id, id("DBINE_TEST_JOBS_OK"));
    assert_eq!(detail.steps.len(), 1);
    assert_eq!(detail.steps[0].kind.as_deref(), Some("PL/SQL"));
    assert!(detail.steps[0]
        .command
        .as_deref()
        .unwrap_or("")
        .contains("dbine ok"));
    assert!(detail
        .properties
        .iter()
        .any(|p| p.label == "Clase" && p.value == "DEFAULT_JOB_CLASS"));
    let chain = s.job_detail(&id("DBINE_TEST_JOBS_CHAIN")).await.unwrap();
    assert_eq!(
        chain
            .steps
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["S1", "S2"],
        "in the order the rules run them"
    );
    assert_eq!(chain.steps[1].kind.as_deref(), Some("PL/SQL"));
    assert!(chain.steps[1]
        .command
        .as_deref()
        .unwrap_or("")
        .contains("step boom"));
    assert_eq!(
        chain
            .properties
            .iter()
            .filter(|p| p.group == "Reglas de la cadena")
            .count(),
        3
    );

    // Run now, and the history after it.
    for name in [
        "DBINE_TEST_JOBS_OK",
        "DBINE_TEST_JOBS_FAIL",
        "DBINE_TEST_JOBS_CHAIN",
    ] {
        s.run_job(&id(name)).await.unwrap();
    }
    let run = finished(&mut s, &id("DBINE_TEST_JOBS_OK")).await;
    eprintln!("{run:#?}");
    assert_eq!(run.outcome, JobOutcome::Succeeded);
    assert_eq!(run.trigger.as_deref(), Some("manual"));
    assert!(rfc3339(&run.started) && run.duration_ms.is_some() && run.id.is_some());
    assert!(
        run.message.as_deref().unwrap_or("").contains("dbine ok"),
        "its output: {run:?}"
    );
    let run = finished(&mut s, &id("DBINE_TEST_JOBS_FAIL")).await;
    assert_eq!(run.outcome, JobOutcome::Failed);
    assert!(
        run.message.as_deref().unwrap_or("").contains("dbine boom"),
        "{run:?}"
    );
    let run = finished(&mut s, &id("DBINE_TEST_JOBS_CHAIN")).await;
    eprintln!("{run:#?}");
    assert_eq!(
        run.steps
            .iter()
            .map(|s| (s.name.as_str(), s.outcome))
            .collect::<Vec<_>>(),
        [("S1", JobOutcome::Succeeded), ("S2", JobOutcome::Failed)]
    );
    assert!(run.steps[1]
        .message
        .as_deref()
        .unwrap_or("")
        .contains("step boom"));
    let list = s.jobs().await.unwrap().jobs;
    let ok = find(&list, &id("DBINE_TEST_JOBS_OK"));
    assert_eq!(ok.last_outcome, Some(JobOutcome::Succeeded));
    assert!(rfc3339(&ok.last_started) && ok.last_duration_ms.is_some());
    assert_eq!(
        find(&list, &id("DBINE_TEST_JOBS_FAIL")).last_outcome,
        Some(JobOutcome::Failed)
    );
    assert!(s
        .job_runs(&id("DBINE_TEST_JOBS_OK"), 0)
        .await
        .unwrap()
        .is_empty());

    // Stop a run.
    s.run_job(&id("DBINE_TEST_JOBS_SLOW")).await.unwrap();
    let slow = wait_running(&mut s, &id("DBINE_TEST_JOBS_SLOW")).await;
    assert!(slow.can_stop && !slow.can_run, "{slow:?}");
    assert_eq!(slow.last_outcome, Some(JobOutcome::Running));
    let runs = s.job_runs(&id("DBINE_TEST_JOBS_SLOW"), 10).await.unwrap();
    assert_eq!(runs[0].outcome, JobOutcome::Running);
    assert!(rfc3339(&runs[0].started), "{runs:?}");
    let again = s
        .run_job(&id("DBINE_TEST_JOBS_SLOW"))
        .await
        .unwrap_err()
        .to_string();
    assert!(again.contains("ya se está ejecutando"), "{again}");
    s.stop_job(&id("DBINE_TEST_JOBS_SLOW")).await.unwrap();
    let run = finished(&mut s, &id("DBINE_TEST_JOBS_SLOW")).await;
    eprintln!("{run:#?}");
    assert_eq!(run.outcome, JobOutcome::Canceled);
    assert!(
        run.message.as_deref().unwrap_or("").contains("Stop job"),
        "{run:?}"
    );
    let not_running = s
        .stop_job(&id("DBINE_TEST_JOBS_SLOW"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        not_running.contains("no se está ejecutando"),
        "{not_running}"
    );

    // Disable and enable.
    s.set_job_enabled(&id("DBINE_TEST_JOBS_OK"), false)
        .await
        .unwrap();
    let off = job(&mut s, &id("DBINE_TEST_JOBS_OK")).await;
    assert!(!off.enabled && off.next_run.is_none(), "{off:?}");
    s.set_job_enabled(&id("DBINE_TEST_JOBS_OK"), true)
        .await
        .unwrap();
    let on = job(&mut s, &id("DBINE_TEST_JOBS_OK")).await;
    assert!(on.enabled && rfc3339(&on.next_run), "{on:?}");

    // The script (DBMS_METADATA): run after dropping the job, it's back.
    let script = s.job_script(&id("DBINE_TEST_JOBS_FAIL")).await.unwrap();
    eprintln!("{script}");
    assert!(
        script.to_lowercase().contains(&format!(
            "create_job('\"{}\".\"dbine_test_jobs_fail\"'",
            me.to_lowercase()
        )),
        "qualified: {script}"
    );
    exec(
        &mut s,
        "BEGIN DBMS_SCHEDULER.DROP_JOB('DBINE_TEST_JOBS_FAIL'); END;",
    )
    .await;
    exec(&mut s, &script).await;
    let back = s.job_detail(&id("DBINE_TEST_JOBS_FAIL")).await.unwrap();
    assert!(!back.job.enabled);
    assert!(back.steps[0]
        .command
        .as_deref()
        .unwrap_or("")
        .contains("dbine boom"));
    // Oracle's own jobs: built from the dictionary.
    let purge = s.job_script("SYS.PURGE_LOG").await.unwrap();
    eprintln!("{purge}");
    assert!(
        purge.contains("DBMS_SCHEDULER.CREATE_JOB(") && purge.contains("'\"SYS\".\"PURGE_LOG\"'"),
        "{purge}"
    );

    // DBMS_JOB jobs are Scheduler jobs (DBMS_JOB$_n) from 19c.
    exec(
        &mut s,
        "DECLARE j BINARY_INTEGER; BEGIN DBMS_JOB.SUBMIT(j, 'NULL; /* dbine_test_jobs_legacy */', SYSDATE + 1, 'SYSDATE + 1'); COMMIT; END;",
    )
    .await;
    let mut legacy = None;
    for j in s.jobs().await.unwrap().jobs {
        if j.kind == "DBMS_JOB" && j.group.as_deref() == Some(me.as_str()) {
            let detail = s.job_detail(&j.id).await.unwrap();
            if detail.steps[0]
                .command
                .as_deref()
                .unwrap_or("")
                .contains("dbine_test_jobs_legacy")
            {
                legacy = Some(j);
            }
        }
    }
    let legacy = legacy.expect("the DBMS_JOB job is listed");
    assert!(legacy.name.starts_with("DBMS_JOB$_"), "{legacy:?}");
    assert_eq!(legacy.schedule.as_deref(), Some("SYSDATE + 1"));
    assert!(legacy.enabled && legacy.can_enable && legacy.can_run);

    // Ids that aren't what the driver produced.
    for bad in [
        "SYSTEM.DBINE_TEST_JOBS_OK'; DROP TABLE x --",
        "SYSTEM.DBINE_TEST_JOBS_OK, SYSTEM.DBINE_TEST_JOBS_FAIL",
        "SYSTEM.DBINE_TEST_JOBS_OK,SYSTEM.DBINE_TEST_JOBS_FAIL",
        "\"SYSTEM\".\"DBINE_TEST_JOBS_OK,SYSTEM.DBINE_TEST_JOBS_FAIL\"",
        "\"SYSTEM\".\"DBINE_TEST_JOBS_OK\" , \"SYS\".\"PURGE_LOG\"",
        "DBMS_JOB:1 OR 1=1",
        "system.dbine_test_jobs_ok",
        // Not the shape the list gives for this job.
        "\"SYSTEM\".\"DBINE_TEST_JOBS_OK\"",
        "",
    ] {
        assert!(s.run_job(bad).await.is_err(), "{bad}");
        assert!(s.set_job_enabled(bad, false).await.is_err(), "{bad}");
        assert!(s.stop_job(bad).await.is_err(), "{bad}");
        assert!(s.job_detail(bad).await.is_err(), "{bad}");
        assert!(s.job_runs(bad, 5).await.is_err(), "{bad}");
        assert!(s.job_script(bad).await.is_err(), "{bad}");
    }
    let ok = job(&mut s, &id("DBINE_TEST_JOBS_OK")).await;
    assert!(ok.enabled, "nothing reached the job");
    let missing = s
        .run_job(&id("DBINE_TEST_JOBS_NONE"))
        .await
        .unwrap_err()
        .to_string();
    assert!(missing.contains("no existe"), "{missing}");

    // A user with only CREATE JOB: its own jobs and the ones it was granted.
    exec(
        &mut s,
        &format!("CREATE USER {USER} IDENTIFIED BY \"{USER_PASSWORD}\""),
    )
    .await;
    exec(
        &mut s,
        &format!("GRANT CREATE SESSION, CREATE JOB TO {USER}"),
    )
    .await;
    exec(
        &mut s,
        &format!(
            "BEGIN DBMS_SCHEDULER.CREATE_JOB('{USER}.DBINE_TEST_JOBS_MINE', job_type => 'PLSQL_BLOCK', \
             job_action => 'BEGIN NULL; END;', enabled => FALSE, auto_drop => FALSE); END;"
        ),
    )
    .await;
    exec(
        &mut s,
        &format!("GRANT ALTER ON {me}.DBINE_TEST_JOBS_FAIL TO {USER}"),
    )
    .await;
    let mine_id = format!("{USER}.DBINE_TEST_JOBS_MINE");
    let theirs = job(&mut s, &mine_id).await;
    assert!(
        theirs.can_run && theirs.can_enable,
        "CREATE ANY JOB reaches a user's job: {theirs:?}"
    );
    let mut user_cfg = cfg.clone();
    user_cfg.username = Some(USER.into());
    user_cfg.password = Some(USER_PASSWORD.into());
    let mut u = d.connect(&user_cfg, None).await.unwrap();
    let list = u.jobs().await.unwrap();
    eprintln!("{:?}", list.notes);
    assert!(
        list.notes.iter().any(|n| n.contains("DBA_SCHEDULER_JOBS")),
        "the list says it's partial: {:?}",
        list.notes
    );
    let mine = find(&list.jobs, &mine_id);
    assert!(mine.can_run && mine.can_enable);
    let granted = find(&list.jobs, &id("DBINE_TEST_JOBS_FAIL"));
    assert!(granted.can_run && granted.can_enable, "ALTER on it");
    assert!(list.jobs.iter().all(|j| j.id != id("DBINE_TEST_JOBS_OK")));
    assert!(u.job_detail(&id("DBINE_TEST_JOBS_OK")).await.is_err());
    u.run_job(&mine_id).await.unwrap();
    assert_eq!(
        finished(&mut u, &mine_id).await.outcome,
        JobOutcome::Succeeded
    );
    // Without SELECT_CATALOG_ROLE, another user's script comes from the
    // dictionary; run by the admin after dropping the job, it's back.
    let built = u.job_script(&id("DBINE_TEST_JOBS_FAIL")).await.unwrap();
    eprintln!("{built}");
    assert!(built.contains("DBMS_SCHEDULER.CREATE_JOB("), "{built}");

    // With CREATE ANY JOB: the jobs of SYSTEM (a schema Oracle maintains,
    // where DBAs make their own) too, Oracle's own jobs not.
    exec(&mut s, &format!("GRANT CREATE ANY JOB TO {USER}")).await;
    let list = u.jobs().await.unwrap();
    let ok = find(&list.jobs, &id("DBINE_TEST_JOBS_OK"));
    assert!(ok.can_run && ok.can_enable, "{ok:?}");
    if let Some(sys) = list.jobs.iter().find(|j| j.id == "SYS.PURGE_LOG") {
        assert!(!sys.can_run && !sys.can_enable, "{sys:?}");
    }
    u.run_job(&id("DBINE_TEST_JOBS_OK")).await.unwrap();
    // A lightweight job's script from the dictionary keeps its style.
    let lightweight = u.job_script(&id("DBINE_TEST_JOBS_LW")).await.unwrap();
    eprintln!("{lightweight}");
    assert!(
        lightweight.contains("job_style => 'LIGHTWEIGHT'"),
        "{lightweight}"
    );
    drop(u);

    exec(
        &mut s,
        "BEGIN DBMS_SCHEDULER.DROP_JOB('DBINE_TEST_JOBS_FAIL'); END;",
    )
    .await;
    exec(&mut s, &built).await;
    let back = s.job_detail(&id("DBINE_TEST_JOBS_FAIL")).await.unwrap();
    assert!(!back.job.enabled && back.job.schedule.is_none());
    assert!(back.steps[0]
        .command
        .as_deref()
        .unwrap_or("")
        .contains("dbine boom"));
    assert!(back
        .properties
        .iter()
        .any(|p| p.label == "Se borra al terminar" && p.value == "No"));
    exec(
        &mut s,
        "BEGIN DBMS_SCHEDULER.DROP_JOB('DBINE_TEST_JOBS_LW'); END;",
    )
    .await;
    exec(&mut s, &lightweight).await;
    let back = s.job_detail(&id("DBINE_TEST_JOBS_LW")).await.unwrap();
    assert!(
        back.properties
            .iter()
            .any(|p| p.label == "Estilo" && p.value == "LIGHTWEIGHT"),
        "{back:?}"
    );
    assert_eq!(back.job.schedule.as_deref(), Some("FREQ=DAILY;BYHOUR=4"));

    // With job_queue_processes at 0 nothing runs: the list says so and no
    // job offers to run. Put back before asserting.
    let queue = scalar(&mut s, QUEUE).await;
    exec(
        &mut s,
        "ALTER SYSTEM SET job_queue_processes = 0 SCOPE = MEMORY",
    )
    .await;
    let list = s.jobs().await;
    let detail = s.job_detail(&id("DBINE_TEST_JOBS_OK")).await;
    let refused = s.run_job(&id("DBINE_TEST_JOBS_OK")).await;
    exec(
        &mut s,
        &format!("ALTER SYSTEM SET job_queue_processes = {queue} SCOPE = MEMORY"),
    )
    .await;
    let list = list.unwrap();
    assert!(
        list.notes
            .iter()
            .any(|n| n.starts_with("job_queue_processes es 0")),
        "{:?}",
        list.notes
    );
    let ok = find(&list.jobs, &id("DBINE_TEST_JOBS_OK"));
    assert!(!ok.can_run && ok.can_enable, "{ok:?}");
    assert!(!detail.unwrap().job.can_run);
    let refused = refused.unwrap_err().to_string();
    assert!(refused.contains("no está disponible"), "{refused}");
}

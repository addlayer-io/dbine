//! Oracle's Scheduler managed through the scheduler contract, against a
//! real server (`DBINE_TEST_ORACLE_ADMIN_URL`, an admin such as SYSTEM):
//! every kind created, read back, edited part by part (checked in the
//! dictionary), its script with secrets masked, and deleted; start a
//! chain job at a step, clear a job's history, the logs; the kinds'
//! actions (open and close a window, run a program and a chain once, stop
//! a running job by force, its own and, with the SYSDBA variable below,
//! one of SYS's, end a detached run); what each object allows
//! (Oracle's own are shown, not changed); the fields, lists and tabs that
//! show only for some modes and types; a login without MANAGE SCHEDULER, a
//! read-only connection, a stale form and ids that aren't what the driver
//! gave. Skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//! DBINE_TEST_ORACLE_SYSDBA_URL=oracle://sys:Secret123@localhost:25601/FREE \
//!   cargo test -p dbine-driver-oracle --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! A chain needs two job slaves (the chain's job and its step's). The Free
//! image runs one (`job_queue_processes` = 1 in the CDB, which caps every
//! PDB): with `DBINE_TEST_ORACLE_SYSDBA_URL` (SYS, connected as SYSDBA to
//! the CDB) the test raises it for its run and puts it back; without it,
//! and with fewer than two slaves, starting a chain at a step and running a
//! job before clearing its history are skipped (the one slave may be busy
//! with the notifications of an earlier run's queues).
//!
//! Every object it makes is named `DBINE_TEST_SCHED_*` and is dropped at
//! the end, whether the test passed or not (SYS's job by SYS); the
//! Scheduler attributes it changes are put back, and the job slaves once
//! AQ's notification job left by the event-based objects has ended.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{
    ConnectionConfig, FormValues, ObjectForm, QueryOutcome, SchedulerItem, SchedulerKind, Session,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const USER: &str = "DBINE_TEST_SCHED_USER";
const USER_PASSWORD: &str = "DbineSched_2026";
const SECRET: &str = "S3cret'pw_dbine";
const SECRET2: &str = "Other#2_dbine";

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

/// SYS as SYSDBA on the CDB, to give the server the job slaves a chain
/// needs.
fn sysdba_config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORACLE_SYSDBA_URL").ok()?;
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
    cfg.options.insert("role".into(), "sysdba".into());
    Some(cfg)
}

/// SYS as SYSDBA on the test's PDB (the admin's service), to own a job
/// there as Oracle's own jobs are owned.
fn sys_pdb_config(cfg: &ConnectionConfig) -> Option<ConnectionConfig> {
    let mut sys = sysdba_config()?;
    sys.options
        .insert("service".into(), cfg.options.get("service")?.clone());
    Some(sys)
}

async fn connect(cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_oracle::drivers().remove(0);
    d.connect(cfg, None).await.expect("connect")
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 100, &mut out).await {
        panic!("{sql}\n→ {e}");
    }
    assert!(out.errors.is_empty(), "{sql}\n→ {:?}", out.errors);
}

async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n→ {e}"));
    out.results
        .iter()
        .rfind(|r| !r.columns.is_empty())
        .map(|r| r.rows.clone())
        .unwrap_or_default()
}

fn text(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| {
        if v.is_null() {
            String::new()
        } else {
            v.to_string()
        }
    })
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let r = rows(s, sql).await;
    r.first()
        .and_then(|row| row.first())
        .map(text)
        .unwrap_or_default()
}

/// Drops everything the test made, by its prefix only.
const CLEANUP: &str = r#"BEGIN
  FOR r IN (SELECT owner, job_name FROM dba_scheduler_jobs
             WHERE job_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\' OR job_name LIKE 'RUN\_CHAIN$DBINE\_%' ESCAPE '\'
                OR job_name LIKE 'RUN\_PROG$DBINE\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_JOB('"' || r.owner || '"."' || r.job_name || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, incompatibility_name n FROM dba_scheduler_incompats
             WHERE incompatibility_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_INCOMPATIBILITY('"' || r.owner || '"."' || r.n || '"');
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, resource_name n FROM dba_scheduler_resources
             WHERE resource_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_RESOURCE('"' || r.owner || '"."' || r.n || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, chain_name n FROM dba_scheduler_chains
             WHERE chain_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_CHAIN('"' || r.owner || '"."' || r.n || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, program_name n FROM dba_scheduler_programs
             WHERE program_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_PROGRAM('"' || r.owner || '"."' || r.n || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, group_name n FROM dba_scheduler_groups
             WHERE group_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_GROUP('"' || r.owner || '"."' || r.n || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, window_name n FROM dba_scheduler_windows
             WHERE window_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_WINDOW('"' || r.owner || '"."' || r.n || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, file_watcher_name n FROM dba_scheduler_file_watchers
             WHERE file_watcher_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_FILE_WATCHER('"' || r.owner || '"."' || r.n || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, schedule_name n FROM dba_scheduler_schedules
             WHERE schedule_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_SCHEDULE('"' || r.owner || '"."' || r.n || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, job_class_name n FROM dba_scheduler_job_classes
             WHERE job_class_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_SCHEDULER.DROP_JOB_CLASS('"' || r.owner || '"."' || r.n || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, credential_name n FROM dba_credentials
             WHERE credential_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_CREDENTIAL.DROP_CREDENTIAL('"' || r.owner || '"."' || r.n || '"', force => TRUE);
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT DISTINCT owner, job_name FROM dba_scheduler_job_log
             WHERE job_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\' OR job_name LIKE 'RUN\_CHAIN$DBINE\_%' ESCAPE '\'
                OR job_name LIKE 'RUN\_PROG$DBINE\_%' ESCAPE '\') LOOP
    DBMS_SCHEDULER.PURGE_LOG(log_history => 0, which_log => 'JOB_LOG',
      job_name => '"' || r.owner || '"."' || r.job_name || '"');
  END LOOP;
  FOR r IN (SELECT owner, name FROM dba_queues
             WHERE name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    BEGIN DBMS_AQADM.STOP_QUEUE('"' || r.owner || '"."' || r.name || '"');
    EXCEPTION WHEN OTHERS THEN NULL; END;
  END LOOP;
  FOR r IN (SELECT owner, queue_table FROM dba_queue_tables
             WHERE queue_table LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    DBMS_AQADM.DROP_QUEUE_TABLE('"' || r.owner || '"."' || r.queue_table || '"', force => TRUE);
  END LOOP;
  FOR r IN (SELECT owner, object_type, object_name FROM dba_objects
             WHERE object_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\'
               AND object_type IN ('PROCEDURE', 'TYPE')) LOOP
    EXECUTE IMMEDIATE 'DROP ' || r.object_type || ' "' || r.owner || '"."' || r.object_name || '"'
      || CASE WHEN r.object_type = 'TYPE' THEN ' FORCE' END;
  END LOOP;
  FOR s IN (SELECT sid, serial# AS serial FROM v$session WHERE username = 'DBINE_TEST_SCHED_USER') LOOP
    BEGIN
      EXECUTE IMMEDIATE 'ALTER SYSTEM KILL SESSION ''' || s.sid || ',' || s.serial || ''' IMMEDIATE';
    EXCEPTION WHEN OTHERS THEN NULL;
    END;
  END LOOP;
  FOR u IN (SELECT username FROM dba_users WHERE username = 'DBINE_TEST_SCHED_USER') LOOP
    EXECUTE IMMEDIATE 'DROP USER DBINE_TEST_SCHED_USER CASCADE';
  END LOOP;
END;"#;

/// Drops the test's job in SYS's schema and its log, as SYS (on the PDB):
/// the admin can't drop SYS's jobs. Before [`CLEANUP`], which would purge
/// that log by an owner-qualified name.
const SYS_CLEANUP: &str = r#"BEGIN
  FOR r IN (SELECT job_name FROM dba_scheduler_jobs
             WHERE owner = 'SYS' AND job_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    DBMS_SCHEDULER.DROP_JOB('"SYS"."' || r.job_name || '"', force => TRUE);
  END LOOP;
  FOR r IN (SELECT DISTINCT job_name FROM dba_scheduler_job_log
             WHERE owner = 'SYS' AND job_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\') LOOP
    DBMS_SCHEDULER.PURGE_LOG(log_history => 0, which_log => 'JOB_LOG', job_name => r.job_name);
  END LOOP;
END;"#;

/// [`SYS_CLEANUP`], where `DBINE_TEST_ORACLE_SYSDBA_URL` is set; an error
/// is returned, so the rest of the cleanup still runs.
async fn sys_cleanup(cfg: &ConnectionConfig) -> Result<(), String> {
    let Some(sys) = sys_pdb_config(cfg) else {
        return Ok(());
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s = d.connect(&sys, None).await.map_err(|e| e.to_string())?;
    let mut out = QueryOutcome::default();
    s.execute(SYS_CLEANUP, 100, &mut out)
        .await
        .map_err(|e| e.to_string())?;
    if out.errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{:?}", out.errors))
    }
}

/// The PL/SQL notification jobs AQ runs on a job slave (an event-based
/// job's queue leaves one for a while).
const NOTIFYING: &str = r"SELECT COUNT(*) FROM dba_scheduler_running_jobs WHERE job_name LIKE 'AQ$\_PLSQL\_NTFN%' ESCAPE '\'";

/// What's left of the test's objects.
const LEFT: &str = r"SELECT (SELECT COUNT(*) FROM dba_objects WHERE object_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\')
     + (SELECT COUNT(*) FROM dba_scheduler_windows WHERE window_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\')
     + (SELECT COUNT(*) FROM dba_scheduler_job_classes WHERE job_class_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\')
     + (SELECT COUNT(*) FROM dba_scheduler_groups WHERE group_name LIKE 'DBINE\_TEST\_SCHED\_%' ESCAPE '\')
     + (SELECT COUNT(*) FROM dba_scheduler_jobs WHERE job_name LIKE 'RUN\_CHAIN$DBINE\_%' ESCAPE '\')
     + (SELECT COUNT(*) FROM dba_scheduler_jobs WHERE job_name LIKE 'RUN\_PROG$DBINE\_%' ESCAPE '\')
     + (SELECT COUNT(*) FROM dba_scheduler_job_log WHERE job_name LIKE 'RUN\_PROG$DBINE\_%' ESCAPE '\')
     + (SELECT COUNT(*) FROM dba_users WHERE username = 'DBINE_TEST_SCHED_USER') FROM dual";

const QUEUE: &str = "SELECT value FROM v$parameter WHERE name = 'job_queue_processes'";

const GLOBALS: &str = "SELECT attribute_name, value FROM dba_scheduler_global_attribute
  WHERE attribute_name IN ('EMAIL_SERVER', 'LOG_HISTORY', 'EMAIL_SENDER') ORDER BY 1";

/// What the designer would send back for a form as it came: its values,
/// the fields' defaults where a value is missing, and its lists' rows.
fn values_of(form: &ObjectForm) -> FormValues {
    let mut values = form.values.clone();
    for f in form.sections.iter().flat_map(|s| s.fields.iter()) {
        if let Some(d) = &f.default {
            values.entry(f.key.clone()).or_insert_with(|| d.clone());
        }
    }
    let lists = form
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .map(|l| (l.key.clone(), l.rows.clone()))
        .collect();
    FormValues { values, lists }
}

fn set(v: &mut FormValues, k: &str, x: Value) {
    v.values.insert(k.into(), x);
}

fn list(v: &mut FormValues, k: &str, rows: Value) {
    let rows: Vec<BTreeMap<String, Value>> = serde_json::from_value(rows).unwrap();
    v.lists.insert(k.into(), rows);
}

async fn new_form(s: &mut Box<dyn Session>, kind: &str) -> FormValues {
    let f = s
        .scheduler_form(kind, None)
        .await
        .unwrap_or_else(|e| panic!("new {kind} form: {e}"));
    assert!(!f.read_only, "{kind}: {:?}", f.notes);
    values_of(&f)
}

async fn form(s: &mut Box<dyn Session>, kind: &str, id: &str) -> FormValues {
    let f = s
        .scheduler_form(kind, Some(id))
        .await
        .unwrap_or_else(|e| panic!("{kind} {id} form: {e}"));
    values_of(&f)
}

async fn save(s: &mut Box<dyn Session>, kind: &str, id: Option<&str>, v: &FormValues) -> String {
    let script = s
        .scheduler_script(kind, id, v)
        .await
        .unwrap_or_else(|e| panic!("script {kind} {id:?}: {e}"));
    eprintln!("--- {kind} {id:?}\n{script}");
    let r = s.scheduler_save(kind, id, v).await;
    r.unwrap_or_else(|e| panic!("save {kind} {id:?}: {e:?}\n{script}"))
        .expect("an id")
}

/// The form read back from the server saves nothing.
async fn unchanged(s: &mut Box<dyn Session>, kind: &str, id: &str) {
    let v = form(s, kind, id).await;
    let script = s.scheduler_script(kind, Some(id), &v).await.unwrap();
    assert!(
        script.contains("no hay cambios que guardar"),
        "{kind} {id} read back and saved as is changes something:\n{script}"
    );
}

async fn delete(s: &mut Box<dyn Session>, kind: &str, id: &str, call: &str) {
    let script = s.scheduler_delete_script(kind, id).await.unwrap();
    assert!(script.contains(call), "{script}");
    s.scheduler_delete(kind, id)
        .await
        .unwrap_or_else(|e| panic!("delete {kind} {id}: {e}"));
    let left = s.scheduler_items(kind).await.unwrap();
    assert!(
        !left.items.iter().any(|i| i.id == id),
        "{kind} {id} still listed"
    );
}

async fn wait_for(s: &mut Box<dyn Session>, sql: &str, what: &str) {
    let until = Instant::now() + Duration::from_secs(90);
    loop {
        if scalar(s, sql).await.parse::<i64>().unwrap_or(0) > 0 {
            return;
        }
        assert!(Instant::now() < until, "{what} never happened");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn the_scheduler_managed_whole() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    assert!(dbine_driver_oracle::drivers()
        .iter()
        .all(|d| d.capabilities().scheduler));
    let mut admin = connect(&cfg).await;
    sys_cleanup(&cfg).await.expect("SYS's cleanup");
    exec(&mut admin, CLEANUP).await;
    let before = rows(&mut admin, GLOBALS).await;
    // A chain run takes two job slaves: raised on the CDB for the test
    // when it can, put back after.
    let mut cdb = None;
    if let Some(scfg) = sysdba_config() {
        let mut root = connect(&scfg).await;
        let q = scalar(&mut root, QUEUE).await;
        assert!(q.bytes().all(|b| b.is_ascii_digit()), "{q}");
        if q.parse::<i64>().unwrap_or(0) < 4 {
            exec(
                &mut root,
                "ALTER SYSTEM SET job_queue_processes = 4 SCOPE = MEMORY",
            )
            .await;
        }
        cdb = Some((root, q));
    }
    // And on the PDB, which caps it again.
    let queue = scalar(&mut admin, QUEUE).await;
    assert!(queue.bytes().all(|b| b.is_ascii_digit()), "{queue}");
    if cdb.is_some() && queue.parse::<i64>().unwrap_or(0) < 4 {
        exec(
            &mut admin,
            "ALTER SYSTEM SET job_queue_processes = 4 SCOPE = MEMORY",
        )
        .await;
    }
    let slaves = scalar(&mut admin, QUEUE).await.parse::<i64>().unwrap_or(0);
    // In its own task: whatever fails in it, the cleanup below still runs.
    let result = tokio::spawn(scenario(cfg.clone(), slaves >= 2)).await;
    let mut admin = connect(&cfg).await;
    let sys_cleaned = sys_cleanup(&cfg).await;
    let mut out = QueryOutcome::default();
    let cleaned = admin.execute(CLEANUP, 100, &mut out).await;
    // The event-based job and schedule leave AQ's notification job on a
    // slave for a while (it ends some two minutes after its last
    // notification): waited for, bounded, before the slaves go back to
    // what they were, which may be one, so the next suite finds it free.
    let until = Instant::now() + Duration::from_secs(180);
    while scalar(&mut admin, NOTIFYING).await != "0" {
        if Instant::now() >= until {
            eprintln!("AQ's notification job still holds a job slave");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    for row in &before {
        let (attr, value) = (text(&row[0]), text(&row[1]));
        let v = if value.is_empty() {
            "NULL".to_string()
        } else {
            format!("'{}'", value.replace('\'', "''"))
        };
        exec(
            &mut admin,
            &format!("BEGIN DBMS_SCHEDULER.SET_SCHEDULER_ATTRIBUTE('{attr}', {v}); END;"),
        )
        .await;
    }
    if scalar(&mut admin, QUEUE).await != queue {
        exec(
            &mut admin,
            &format!("ALTER SYSTEM SET job_queue_processes = {queue} SCOPE = MEMORY"),
        )
        .await;
    }
    if let Some((mut root, q)) = cdb {
        if scalar(&mut root, QUEUE).await != q {
            exec(
                &mut root,
                &format!("ALTER SYSTEM SET job_queue_processes = {q} SCOPE = MEMORY"),
            )
            .await;
        }
    }
    let left = scalar(&mut admin, LEFT).await;
    let after = rows(&mut admin, GLOBALS).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
    assert_eq!(sys_cleaned, Ok(()), "SYS's cleanup");
    assert!(
        cleaned.is_ok() && out.errors.is_empty(),
        "cleanup: {cleaned:?} {:?}",
        out.errors
    );
    assert_eq!(left, "0", "objects left behind");
    assert_eq!(before, after, "Scheduler attributes not put back");
}

async fn scenario(cfg: ConnectionConfig, chains_run: bool) {
    let mut s = connect(&cfg).await;
    let info = s.scheduler_info().await.unwrap();
    let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
    for k in [
        "job",
        "program",
        "schedule",
        "chain",
        "file_watcher",
        "credential",
        "job_class",
        "window",
        "window_group",
        "db_destination",
        "external_destination",
        "destination_group",
        "resource",
        "incompatibility",
        "global",
    ] {
        assert!(keys.contains(&k), "{k} in {keys:?}");
    }
    assert_eq!(keys[0], "job");
    assert!(info.logs);
    assert!(info
        .kinds
        .iter()
        .filter(|k| k.key != "external_destination" && k.key != "global")
        .all(|k| k.can_create && k.can_edit && k.can_delete));

    // What the test's objects need.
    exec(
        &mut s,
        "CREATE OR REPLACE PROCEDURE dbine_test_sched_proc(p_n NUMBER, p_job VARCHAR2) AS BEGIN NULL; END;",
    )
    .await;
    exec(
        &mut s,
        "CREATE OR REPLACE TYPE dbine_test_sched_t AS OBJECT (x NUMBER)",
    )
    .await;
    exec(
        &mut s,
        "BEGIN
           DBMS_AQADM.CREATE_QUEUE_TABLE(queue_table => 'DBINE_TEST_SCHED_QT',
             queue_payload_type => 'DBINE_TEST_SCHED_T', multiple_consumers => TRUE);
           DBMS_AQADM.CREATE_QUEUE('DBINE_TEST_SCHED_Q', 'DBINE_TEST_SCHED_QT');
           DBMS_AQADM.START_QUEUE('DBINE_TEST_SCHED_Q');
         END;",
    )
    .await;

    let cred = credentials(&mut s).await;
    let (prog, prog2) = programs(&mut s).await;
    // Runs first: an event-based job keeps a job slave busy with its
    // queue's notifications afterwards.
    runs(&mut s, &prog2, chains_run).await;
    actions(&cfg, &mut s, &prog2, chains_run).await;
    let (cal, evt) = schedules(&mut s).await;
    let chain = chains(&mut s, &prog2, &evt).await;
    let class = classes(&mut s).await;
    let (win, wgrp) = windows(&mut s).await;
    let fw = watchers(&mut s, &cred).await;
    let (job, job2, chainjob) = jobs(&mut s, &prog, &cal, &win, &fw, &class, &chain).await;
    let (res, inc) = resources(&mut s, &job, &job2, &chainjob).await;
    let dgrp = destinations(&mut s, &cred).await;
    globals_and_logs(&mut s, &job).await;
    guards(&cfg, &mut s, &job).await;
    abilities(&mut s, &job, &class).await;
    conditions(&mut s).await;

    // Deleted, each through its own script.
    delete(&mut s, "incompatibility", &inc, "DROP_INCOMPATIBILITY").await;
    delete(&mut s, "resource", &res, "DROP_RESOURCE").await;
    for j in [&job, &job2, &chainjob] {
        delete(&mut s, "job", j, "DROP_JOB").await;
    }
    delete(&mut s, "chain", &chain, "DROP_CHAIN").await;
    delete(&mut s, "program", &prog, "DROP_PROGRAM").await;
    delete(&mut s, "program", &prog2, "DROP_PROGRAM").await;
    for sch in [&cal, &evt] {
        delete(&mut s, "schedule", sch, "DROP_SCHEDULE").await;
    }
    delete(&mut s, "window_group", &wgrp, "DROP_GROUP").await;
    delete(&mut s, "window", &win, "DROP_WINDOW").await;
    delete(&mut s, "job_class", &class, "DROP_JOB_CLASS").await;
    delete(&mut s, "file_watcher", &fw, "DROP_FILE_WATCHER").await;
    delete(&mut s, "destination_group", &dgrp, "DROP_GROUP").await;
    delete(&mut s, "credential", &cred, "DROP_CREDENTIAL").await;
}

async fn credentials(s: &mut Box<dyn Session>) -> String {
    let mut v = new_form(s, "credential").await;
    set(&mut v, "name", json!("dbine_test_sched_cred"));
    set(&mut v, "username", json!("oracle"));
    set(&mut v, "password", json!(SECRET));
    set(&mut v, "comments", json!("it's a test"));
    let script = s.scheduler_script("credential", None, &v).await.unwrap();
    assert!(script.contains("'********'"), "{script}");
    assert!(!script.contains("S3cret"), "{script}");
    let cred = save(s, "credential", None, &v).await;
    assert_eq!(cred, "SYSTEM.DBINE_TEST_SCHED_CRED");
    assert_eq!(
        scalar(s, "SELECT username || '|' || comments || '|' || enabled FROM dba_credentials WHERE owner = 'SYSTEM' AND credential_name = 'DBINE_TEST_SCHED_CRED'").await,
        "oracle|it's a test|TRUE"
    );
    // The password was bound, never written into a statement.
    assert_eq!(
        scalar(s, "SELECT COUNT(*) FROM v$sql WHERE sql_fulltext LIKE '%S3cret%' AND sql_fulltext NOT LIKE '%v$sql%'").await,
        "0"
    );
    unchanged(s, "credential", &cred).await;
    let f = s.scheduler_form("credential", Some(&cred)).await.unwrap();
    assert_eq!(f.values.get("password"), Some(&Value::Null));
    let mut v = values_of(&f);
    set(&mut v, "username", json!("oracle2"));
    set(&mut v, "password", json!(SECRET2));
    set(&mut v, "enabled", json!(false));
    set(&mut v, "database_role", json!("SYSDBA"));
    let script = s
        .scheduler_script("credential", Some(&cred), &v)
        .await
        .unwrap();
    assert!(
        script.contains("UPDATE_CREDENTIAL") && script.contains("DISABLE_CREDENTIAL"),
        "{script}"
    );
    assert!(!script.contains("Other#2"), "{script}");
    save(s, "credential", Some(&cred), &v).await;
    assert_eq!(
        scalar(s, "SELECT c.username || '|' || c.enabled || '|' || sc.database_role FROM dba_credentials c JOIN dba_scheduler_credentials sc ON sc.owner = c.owner AND sc.credential_name = c.credential_name WHERE c.owner = 'SYSTEM' AND c.credential_name = 'DBINE_TEST_SCHED_CRED'").await,
        "oracle2|FALSE|SYSDBA"
    );
    unchanged(s, "credential", &cred).await;
    // Back on, for the file watcher.
    let mut v = form(s, "credential", &cred).await;
    set(&mut v, "enabled", json!(true));
    set(&mut v, "database_role", json!(""));
    save(s, "credential", Some(&cred), &v).await;
    cred
}

async fn programs(s: &mut Box<dyn Session>) -> (String, String) {
    let mut v = new_form(s, "program").await;
    set(&mut v, "name", json!("dbine_test_sched_prog"));
    set(&mut v, "program_type", json!("STORED_PROCEDURE"));
    set(&mut v, "procedure", json!("SYSTEM.DBINE_TEST_SCHED_PROC"));
    set(&mut v, "comments", json!("dbine program"));
    list(
        &mut v,
        "arguments",
        json!([
            {"name": "p_n", "metadata": "VALUE", "arg_type": "NUMBER", "default": "5"},
            {"name": "p_job", "metadata": "JOB_NAME"}
        ]),
    );
    let prog = save(s, "program", None, &v).await;
    assert_eq!(prog, "SYSTEM.DBINE_TEST_SCHED_PROG");
    assert_eq!(
        scalar(s, "SELECT program_type || '|' || program_action || '|' || number_of_arguments || '|' || enabled FROM dba_scheduler_programs WHERE owner = 'SYSTEM' AND program_name = 'DBINE_TEST_SCHED_PROG'").await,
        "STORED_PROCEDURE|SYSTEM.DBINE_TEST_SCHED_PROC|2|TRUE"
    );
    assert_eq!(
        scalar(s, "SELECT LISTAGG(argument_position || ':' || argument_name || ':' || argument_type || ':' || metadata_attribute || ':' || default_value, ',') WITHIN GROUP (ORDER BY argument_position) FROM dba_scheduler_program_args WHERE owner = 'SYSTEM' AND program_name = 'DBINE_TEST_SCHED_PROG'").await,
        "1:P_N:NUMBER::5,2:P_JOB::JOB_NAME:<NO VALUE>"
    );
    unchanged(s, "program", &prog).await;
    let mut v = form(s, "program", &prog).await;
    list(
        &mut v,
        "arguments",
        json!([{"name": "P_N", "metadata": "VALUE", "arg_type": "NUMBER", "default": "7"}]),
    );
    set(&mut v, "detached", json!(true));
    let script = s
        .scheduler_script("program", Some(&prog), &v)
        .await
        .unwrap();
    assert!(
        script.contains("DROP_PROGRAM_ARGUMENT") && script.contains("DBMS_SCHEDULER.DISABLE"),
        "{script}"
    );
    save(s, "program", Some(&prog), &v).await;
    assert_eq!(
        scalar(s, "SELECT p.number_of_arguments || '|' || p.detached || '|' || p.enabled || '|' || a.default_value FROM dba_scheduler_programs p JOIN dba_scheduler_program_args a ON a.owner = p.owner AND a.program_name = p.program_name WHERE p.owner = 'SYSTEM' AND p.program_name = 'DBINE_TEST_SCHED_PROG'").await,
        "1|TRUE|TRUE|7"
    );
    unchanged(s, "program", &prog).await;
    // Back to attached, for the job that runs it.
    let mut v = form(s, "program", &prog).await;
    set(&mut v, "detached", json!(false));
    save(s, "program", Some(&prog), &v).await;

    let mut v = new_form(s, "program").await;
    set(&mut v, "name", json!("dbine_test_sched_prog2"));
    set(&mut v, "plsql", json!("BEGIN DBMS_SESSION.SLEEP(3); END;"));
    let prog2 = save(s, "program", None, &v).await;
    unchanged(s, "program", &prog2).await;
    (prog, prog2)
}

async fn schedules(s: &mut Box<dyn Session>) -> (String, String) {
    let mut v = new_form(s, "schedule").await;
    set(&mut v, "name", json!("dbine_test_sched_cal"));
    set(&mut v, "schedule_mode", json!("calendar"));
    set(&mut v, "freq", json!("WEEKLY"));
    set(&mut v, "byday", json!(["MON", "WED"]));
    set(&mut v, "byhour", json!("3"));
    set(&mut v, "byminute", json!("30"));
    set(&mut v, "start_date", json!("2030-01-01T00:00:00"));
    set(&mut v, "time_zone", json!("America/Argentina/Buenos_Aires"));
    set(&mut v, "comments", json!("dbine schedule"));
    let cal = save(s, "schedule", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT repeat_interval || '|' || TO_CHAR(start_date, 'YYYY-MM-DD HH24:MI:SS TZR') FROM dba_scheduler_schedules WHERE owner = 'SYSTEM' AND schedule_name = 'DBINE_TEST_SCHED_CAL'").await,
        "FREQ=WEEKLY;BYDAY=MON,WED;BYHOUR=3;BYMINUTE=30|2030-01-01 00:00:00 AMERICA/ARGENTINA/BUENOS_AIRES"
    );
    unchanged(s, "schedule", &cal).await;
    let mut v = form(s, "schedule", &cal).await;
    assert_eq!(v.values["schedule_mode"], json!("calendar"));
    assert_eq!(v.values["byday"], json!(["MON", "WED"]));
    set(&mut v, "byhour", json!("4,16"));
    set(&mut v, "end_date", json!("2031-01-01T00:00:00"));
    let script = s
        .scheduler_script("schedule", Some(&cal), &v)
        .await
        .unwrap();
    assert!(script.contains("'repeat_interval'"), "{script}");
    assert!(
        script.contains("'FREQ=WEEKLY;BYDAY=MON,WED;BYHOUR=4,16;BYMINUTE=30'"),
        "{script}"
    );
    assert!(!script.contains("'start_date'"), "{script}");
    save(s, "schedule", Some(&cal), &v).await;
    assert_eq!(
        scalar(s, "SELECT repeat_interval || '|' || TO_CHAR(end_date, 'YYYY-MM-DD') FROM dba_scheduler_schedules WHERE owner = 'SYSTEM' AND schedule_name = 'DBINE_TEST_SCHED_CAL'").await,
        "FREQ=WEEKLY;BYDAY=MON,WED;BYHOUR=4,16;BYMINUTE=30|2031-01-01"
    );
    unchanged(s, "schedule", &cal).await;

    // An expression the fields can't say stays as it is.
    let mut v = new_form(s, "schedule").await;
    set(&mut v, "name", json!("dbine_test_sched_expr"));
    set(&mut v, "schedule_mode", json!("expression"));
    set(
        &mut v,
        "repeat_interval",
        json!("FREQ=MONTHLY;BYDAY=-1FRI;BYHOUR=22"),
    );
    let expr = save(s, "schedule", None, &v).await;
    let f = s.scheduler_form("schedule", Some(&expr)).await.unwrap();
    assert_eq!(f.values["schedule_mode"], json!("expression"));
    unchanged(s, "schedule", &expr).await;
    // Once, at a date with an offset.
    let mut v = new_form(s, "schedule").await;
    set(&mut v, "name", json!("dbine_test_sched_once"));
    set(&mut v, "schedule_mode", json!("once"));
    set(&mut v, "start_date", json!("2030-06-01T10:00:00"));
    set(&mut v, "time_zone", json!("-03:00"));
    let once = save(s, "schedule", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT schedule_type || '|' || TO_CHAR(start_date, 'TZR') FROM dba_scheduler_schedules WHERE owner = 'SYSTEM' AND schedule_name = 'DBINE_TEST_SCHED_ONCE'").await,
        "ONCE|-03:00"
    );
    unchanged(s, "schedule", &once).await;
    delete(s, "schedule", &expr, "DROP_SCHEDULE").await;
    delete(s, "schedule", &once, "DROP_SCHEDULE").await;

    // By event.
    let mut v = new_form(s, "schedule").await;
    set(&mut v, "name", json!("dbine_test_sched_evt"));
    set(&mut v, "schedule_mode", json!("event"));
    set(&mut v, "event_queue", json!("SYSTEM.DBINE_TEST_SCHED_Q"));
    set(&mut v, "event_condition", json!("tab.user_data.x = 1"));
    let evt = save(s, "schedule", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT schedule_type || '|' || event_queue_name || '|' || event_condition FROM dba_scheduler_schedules WHERE owner = 'SYSTEM' AND schedule_name = 'DBINE_TEST_SCHED_EVT'").await,
        "EVENT|DBINE_TEST_SCHED_Q|tab.user_data.x = 1"
    );
    unchanged(s, "schedule", &evt).await;
    let mut v = form(s, "schedule", &evt).await;
    set(&mut v, "event_condition", json!("tab.user_data.x = 2"));
    save(s, "schedule", Some(&evt), &v).await;
    assert_eq!(
        scalar(s, "SELECT event_condition FROM dba_scheduler_schedules WHERE owner = 'SYSTEM' AND schedule_name = 'DBINE_TEST_SCHED_EVT'").await,
        "tab.user_data.x = 2"
    );
    // A schedule by event doesn't become one by time.
    let mut v = form(s, "schedule", &evt).await;
    set(&mut v, "schedule_mode", json!("calendar"));
    assert!(s.scheduler_save("schedule", Some(&evt), &v).await.is_err());
    (cal, evt)
}

async fn chains(s: &mut Box<dyn Session>, prog2: &str, evt: &str) -> String {
    let mut v = new_form(s, "chain").await;
    set(&mut v, "name", json!("dbine_test_sched_chain"));
    set(&mut v, "evaluation_interval", json!(600));
    set(&mut v, "comments", json!("dbine chain"));
    list(
        &mut v,
        "steps",
        json!([
            {"step_name": "s1", "step_type": "PROGRAM", "program": prog2},
            {"step_name": "s2", "step_type": "PROGRAM", "program": prog2, "pause_before": true},
            {"step_name": "s3", "step_type": "EVENT_SCHEDULE", "event_schedule": evt, "timeout": 60}
        ]),
    );
    list(
        &mut v,
        "rules",
        json!([
            {"rule_name": "r1", "condition": "TRUE", "action": "START S1", "comments": "first"},
            {"condition": "S1 COMPLETED", "action": "START S2"},
            {"rule_name": "r3", "condition": "S2 COMPLETED", "action": "END"}
        ]),
    );
    let chain = save(s, "chain", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT enabled || '|' || number_of_steps || '|' || number_of_rules || '|' || EXTRACT(MINUTE FROM evaluation_interval) || '|' || comments FROM dba_scheduler_chains WHERE owner = 'SYSTEM' AND chain_name = 'DBINE_TEST_SCHED_CHAIN'").await,
        "TRUE|3|3|10|dbine chain"
    );
    assert_eq!(
        scalar(s, "SELECT LISTAGG(step_name || ':' || step_type || ':' || pause_before, ',') WITHIN GROUP (ORDER BY step_name) FROM dba_scheduler_chain_steps WHERE owner = 'SYSTEM' AND chain_name = 'DBINE_TEST_SCHED_CHAIN'").await,
        "S1:PROGRAM:FALSE,S2:PROGRAM:TRUE,S3:EVENT_SCHEDULE:FALSE"
    );
    assert_eq!(
        scalar(s, "SELECT EXTRACT(MINUTE FROM timeout) FROM dba_scheduler_chain_steps WHERE owner = 'SYSTEM' AND chain_name = 'DBINE_TEST_SCHED_CHAIN' AND step_name = 'S3'").await,
        "1"
    );
    unchanged(s, "chain", &chain).await;
    let mut v = form(s, "chain", &chain).await;
    let mut steps = v.lists["steps"].clone();
    steps.retain(|r| r["step_name"] != json!("S3"));
    for r in steps.iter_mut() {
        if r["step_name"] == json!("S2") {
            r.insert("skip".into(), json!(true));
            r.insert("pause_before".into(), json!(false));
        }
    }
    v.lists.insert("steps".into(), steps);
    let mut rules = v.lists["rules"].clone();
    for r in rules.iter_mut() {
        if r["rule_name"] == json!("R3") {
            r.insert("comments".into(), json!("last"));
        }
    }
    rules.push(BTreeMap::from([
        ("rule_name".to_string(), json!("r4")),
        ("condition".to_string(), json!("S2 FAILED")),
        ("action".to_string(), json!("END 1")),
    ]));
    v.lists.insert("rules".into(), rules);
    let script = s.scheduler_script("chain", Some(&chain), &v).await.unwrap();
    assert!(
        script.contains("DROP_CHAIN_STEP") && script.contains("ALTER_CHAIN"),
        "{script}"
    );
    assert!(!script.contains("DEFINE_CHAIN_STEP"), "{script}");
    save(s, "chain", Some(&chain), &v).await;
    assert_eq!(
        scalar(s, "SELECT LISTAGG(step_name || ':' || skip || ':' || pause_before, ',') WITHIN GROUP (ORDER BY step_name) FROM dba_scheduler_chain_steps WHERE owner = 'SYSTEM' AND chain_name = 'DBINE_TEST_SCHED_CHAIN'").await,
        "S1:FALSE:FALSE,S2:TRUE:FALSE"
    );
    assert_eq!(
        scalar(s, "SELECT COUNT(*) || '|' || MAX(CASE WHEN rule_name = 'R3' THEN comments END) || '|' || MAX(CASE WHEN rule_name = 'R4' THEN TRIM(action) END) FROM dba_scheduler_chain_rules WHERE owner = 'SYSTEM' AND chain_name = 'DBINE_TEST_SCHED_CHAIN'").await,
        "4|last|END 1"
    );
    unchanged(s, "chain", &chain).await;
    // A step made quoted, in lower case, outside DBine: read back as
    // itself (nothing to save), and dropped by its own name.
    exec(
        s,
        &format!("BEGIN DBMS_SCHEDULER.DEFINE_CHAIN_STEP('SYSTEM.DBINE_TEST_SCHED_CHAIN', '\"s_q\"', '{prog2}'); END;"),
    )
    .await;
    unchanged(s, "chain", &chain).await;
    let mut v = form(s, "chain", &chain).await;
    let mut steps = v.lists["steps"].clone();
    steps.retain(|r| r["step_name"] != json!("s_q"));
    v.lists.insert("steps".into(), steps);
    let script = s.scheduler_script("chain", Some(&chain), &v).await.unwrap();
    assert!(
        script.contains("step_name => '\"s_q\"'") && !script.contains("DEFINE_CHAIN_STEP"),
        "{script}"
    );
    save(s, "chain", Some(&chain), &v).await;
    assert_eq!(
        scalar(s, "SELECT LISTAGG(step_name, ',') WITHIN GROUP (ORDER BY step_name) FROM dba_scheduler_chain_steps WHERE owner = 'SYSTEM' AND chain_name = 'DBINE_TEST_SCHED_CHAIN'").await,
        "S1,S2"
    );
    // S2 runs again, for the start at a step.
    let mut v = form(s, "chain", &chain).await;
    let mut steps = v.lists["steps"].clone();
    for r in steps.iter_mut() {
        r.insert("skip".into(), json!(false));
    }
    v.lists.insert("steps".into(), steps);
    save(s, "chain", Some(&chain), &v).await;
    chain
}

async fn classes(s: &mut Box<dyn Session>) -> String {
    let mut v = new_form(s, "job_class").await;
    set(&mut v, "name", json!("dbine_test_sched_class"));
    set(&mut v, "logging_level", json!("FULL"));
    set(&mut v, "log_history", json!(10));
    set(&mut v, "comments", json!("dbine class"));
    let class = save(s, "job_class", None, &v).await;
    assert_eq!(class, "SYS.DBINE_TEST_SCHED_CLASS");
    assert_eq!(
        scalar(s, "SELECT logging_level || '|' || log_history || '|' || comments FROM dba_scheduler_job_classes WHERE job_class_name = 'DBINE_TEST_SCHED_CLASS'").await,
        "FULL|10|dbine class"
    );
    unchanged(s, "job_class", &class).await;
    let mut v = form(s, "job_class", &class).await;
    set(&mut v, "logging_level", json!("FAILED_RUNS"));
    set(&mut v, "log_history", json!(null));
    save(s, "job_class", Some(&class), &v).await;
    assert_eq!(
        scalar(s, "SELECT logging_level || '|' || log_history FROM dba_scheduler_job_classes WHERE job_class_name = 'DBINE_TEST_SCHED_CLASS'").await,
        "FAILED RUNS|"
    );
    unchanged(s, "job_class", &class).await;
    class
}

async fn windows(s: &mut Box<dyn Session>) -> (String, String) {
    let mut v = new_form(s, "window").await;
    set(&mut v, "name", json!("dbine_test_sched_win"));
    set(&mut v, "freq", json!("DAILY"));
    set(&mut v, "byhour", json!("1"));
    set(&mut v, "time_zone", json!("UTC"));
    set(&mut v, "start_date", json!("2030-01-01T01:00:00"));
    set(&mut v, "duration", json!(3600));
    set(&mut v, "window_priority", json!("HIGH"));
    set(&mut v, "resource_plan", json!("DEFAULT_PLAN"));
    set(&mut v, "enabled", json!(false));
    set(&mut v, "comments", json!("dbine window"));
    let win = save(s, "window", None, &v).await;
    assert_eq!(win, "SYS.DBINE_TEST_SCHED_WIN");
    assert_eq!(
        scalar(s, "SELECT resource_plan || '|' || repeat_interval || '|' || EXTRACT(HOUR FROM duration) || '|' || window_priority || '|' || enabled FROM dba_scheduler_windows WHERE window_name = 'DBINE_TEST_SCHED_WIN'").await,
        "DEFAULT_PLAN|FREQ=DAILY;BYHOUR=1|1|HIGH|FALSE"
    );
    unchanged(s, "window", &win).await;
    let mut v = form(s, "window", &win).await;
    set(&mut v, "duration", json!(7200));
    set(&mut v, "window_priority", json!("LOW"));
    set(&mut v, "resource_plan", json!(""));
    save(s, "window", Some(&win), &v).await;
    assert_eq!(
        scalar(s, "SELECT resource_plan || '|' || EXTRACT(HOUR FROM duration) || '|' || window_priority FROM dba_scheduler_windows WHERE window_name = 'DBINE_TEST_SCHED_WIN'").await,
        "|2|LOW"
    );
    unchanged(s, "window", &win).await;

    let mut v = new_form(s, "window_group").await;
    set(&mut v, "name", json!("dbine_test_sched_wgrp"));
    set(&mut v, "members", json!([win, "SYS.WEEKNIGHT_WINDOW"]));
    set(&mut v, "comments", json!("dbine group"));
    let wgrp = save(s, "window_group", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT LISTAGG(window_name, ',') WITHIN GROUP (ORDER BY window_name) FROM dba_scheduler_wingroup_members WHERE window_group_name = 'DBINE_TEST_SCHED_WGRP'").await,
        "DBINE_TEST_SCHED_WIN,WEEKNIGHT_WINDOW"
    );
    unchanged(s, "window_group", &wgrp).await;
    let mut v = form(s, "window_group", &wgrp).await;
    set(&mut v, "members", json!([win]));
    set(&mut v, "enabled", json!(false));
    save(s, "window_group", Some(&wgrp), &v).await;
    assert_eq!(
        scalar(s, "SELECT g.enabled || '|' || (SELECT COUNT(*) FROM dba_scheduler_wingroup_members m WHERE m.window_group_name = g.window_group_name) FROM dba_scheduler_window_groups g WHERE g.window_group_name = 'DBINE_TEST_SCHED_WGRP'").await,
        "FALSE|1"
    );
    unchanged(s, "window_group", &wgrp).await;
    (win, wgrp)
}

async fn watchers(s: &mut Box<dyn Session>, cred: &str) -> String {
    let mut v = new_form(s, "file_watcher").await;
    set(&mut v, "name", json!("dbine_test_sched_fw"));
    set(&mut v, "directory_path", json!("/tmp"));
    set(&mut v, "file_name", json!("dbine_*.txt"));
    set(&mut v, "credential", json!(cred));
    set(&mut v, "min_file_size", json!(10));
    set(&mut v, "steady_state_duration", json!(30));
    set(&mut v, "enabled", json!(false));
    let fw = save(s, "file_watcher", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT directory_path || '|' || file_name || '|' || credential_name || '|' || min_file_size || '|' || EXTRACT(SECOND FROM steady_state_duration) || '|' || enabled FROM dba_scheduler_file_watchers WHERE owner = 'SYSTEM' AND file_watcher_name = 'DBINE_TEST_SCHED_FW'").await,
        "/tmp|dbine_*.txt|DBINE_TEST_SCHED_CRED|10|30|FALSE"
    );
    unchanged(s, "file_watcher", &fw).await;
    let mut v = form(s, "file_watcher", &fw).await;
    set(&mut v, "min_file_size", json!(20));
    set(&mut v, "steady_state_duration", json!(null));
    set(&mut v, "comments", json!("dbine watcher"));
    save(s, "file_watcher", Some(&fw), &v).await;
    assert_eq!(
        scalar(s, "SELECT min_file_size || '|' || comments FROM dba_scheduler_file_watchers WHERE owner = 'SYSTEM' AND file_watcher_name = 'DBINE_TEST_SCHED_FW'").await,
        "20|dbine watcher"
    );
    unchanged(s, "file_watcher", &fw).await;
    fw
}

const JOB_ROW: &str = "SELECT job_type || '|' || program_name || '|' || schedule_type || '|' || schedule_owner || '.' || schedule_name || '|' || enabled || '|' || repeat_interval FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB'";

async fn jobs(
    s: &mut Box<dyn Session>,
    prog: &str,
    cal: &str,
    win: &str,
    fw: &str,
    class: &str,
    chain: &str,
) -> (String, String, String) {
    let action = "BEGIN\n  NULL; -- it's dbine\nEND;";
    let mut v = new_form(s, "job").await;
    set(&mut v, "name", json!("dbine_test_sched_job"));
    set(&mut v, "job_type", json!("PLSQL_BLOCK"));
    set(&mut v, "plsql", json!(action));
    set(&mut v, "schedule_mode", json!("calendar"));
    set(&mut v, "freq", json!("DAILY"));
    set(&mut v, "byhour", json!("2"));
    set(&mut v, "start_date", json!("2030-01-01T02:00:00"));
    set(&mut v, "time_zone", json!("UTC"));
    set(&mut v, "job_class", json!(class));
    set(&mut v, "comments", json!("dbine 'test'"));
    set(&mut v, "job_priority", json!(2));
    set(&mut v, "logging_level", json!("FULL"));
    set(&mut v, "max_runs", json!(10));
    set(&mut v, "max_failures", json!(3));
    set(&mut v, "max_run_duration", json!(600));
    set(&mut v, "schedule_limit", json!(3600));
    set(
        &mut v,
        "raise_events",
        json!(["JOB_FAILED", "JOB_SUCCEEDED"]),
    );
    set(&mut v, "restartable", json!(true));
    set(&mut v, "store_output", json!(false));
    let script = s.scheduler_script("job", None, &v).await.unwrap();
    assert!(
        script.contains("created := TRUE;") && script.contains("DROP_JOB"),
        "{script}"
    );
    let job = save(s, "job", None, &v).await;
    assert_eq!(job, "SYSTEM.DBINE_TEST_SCHED_JOB");
    assert_eq!(
        scalar(s, "SELECT job_type || '|' || job_action || '|' || repeat_interval || '|' || job_class || '|' || enabled || '|' || job_priority || '|' || logging_level || '|' || max_runs || '|' || max_failures || '|' || EXTRACT(MINUTE FROM max_run_duration) || '|' || raise_events || '|' || restartable || '|' || store_output || '|' || comments FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB'").await,
        format!("PLSQL_BLOCK|{action}|FREQ=DAILY;BYHOUR=2|DBINE_TEST_SCHED_CLASS|TRUE|2|FULL|10|3|10|JOB_SUCCEEDED,JOB_FAILED,JOB_OVER_MAX_DUR|TRUE|FALSE|dbine 'test'")
    );
    unchanged(s, "job", &job).await;

    // (With a maximum duration Oracle raises JOB_OVER_MAX_DUR too.)
    // A stored procedure with arguments.
    let mut v = form(s, "job", &job).await;
    set(&mut v, "job_type", json!("STORED_PROCEDURE"));
    set(&mut v, "procedure", json!("SYSTEM.DBINE_TEST_SCHED_PROC"));
    list(
        &mut v,
        "arguments",
        json!([{"value": "5"}, {"value": "x'y"}]),
    );
    let script = s.scheduler_script("job", Some(&job), &v).await.unwrap();
    assert!(
        script.contains("disabled := TRUE;"),
        "disabled first: {script}"
    );
    save(s, "job", Some(&job), &v).await;
    assert_eq!(
        scalar(s, "SELECT j.job_type || '|' || j.number_of_arguments || '|' || j.enabled || '|' || (SELECT LISTAGG(a.value, ',') WITHIN GROUP (ORDER BY a.argument_position) FROM dba_scheduler_job_args a WHERE a.owner = j.owner AND a.job_name = j.job_name) FROM dba_scheduler_jobs j WHERE j.owner = 'SYSTEM' AND j.job_name = 'DBINE_TEST_SCHED_JOB'").await,
        "STORED_PROCEDURE|2|TRUE|5,x'y"
    );
    unchanged(s, "job", &job).await;

    // A program, with its argument's value.
    let mut v = form(s, "job", &job).await;
    set(&mut v, "job_type", json!("PROGRAM"));
    set(&mut v, "program", json!(prog));
    list(&mut v, "arguments", json!([{"value": "9"}]));
    save(s, "job", Some(&job), &v).await;
    assert_eq!(
        scalar(s, "SELECT j.program_name || '|' || j.enabled || '|' || (SELECT LISTAGG(a.value, ',') WITHIN GROUP (ORDER BY a.argument_position) FROM dba_scheduler_job_args a WHERE a.owner = j.owner AND a.job_name = j.job_name) FROM dba_scheduler_jobs j WHERE j.owner = 'SYSTEM' AND j.job_name = 'DBINE_TEST_SCHED_JOB'").await,
        "DBINE_TEST_SCHED_PROG|TRUE|9"
    );
    let f = s.scheduler_form("job", Some(&job)).await.unwrap();
    let args = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .find(|l| l.key == "arguments")
        .unwrap();
    // A row's place is its argument's position: not reordered.
    assert!(!args.orderable);
    assert_eq!(args.rows[0]["name"], json!("P_N"));
    unchanged(s, "job", &job).await;
    // A value with U+0001 (where a block keeps a secret until it's
    // rendered): written as CHR(1), saved as typed.
    let mut v = form(s, "job", &job).await;
    set(&mut v, "comments", json!("dbine \u{1} 'test'"));
    save(s, "job", Some(&job), &v).await;
    assert_eq!(
        scalar(s, "SELECT INSTR(comments, CHR(1)) || '|' || LENGTH(comments) FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB'").await,
        "7|14"
    );
    unchanged(s, "job", &job).await;

    // A named schedule, a window, an event, a file watcher, a calendar again.
    let mut v = form(s, "job", &job).await;
    set(&mut v, "schedule_mode", json!("named"));
    set(&mut v, "schedule", json!(cal));
    save(s, "job", Some(&job), &v).await;
    assert_eq!(
        scalar(s, JOB_ROW).await,
        "|DBINE_TEST_SCHED_PROG|NAMED|SYSTEM.DBINE_TEST_SCHED_CAL|TRUE|"
    );
    unchanged(s, "job", &job).await;
    // A calendar string Oracle refuses: the save fails before a call
    // changes anything (still enabled, still on its named schedule).
    let mut v = form(s, "job", &job).await;
    set(&mut v, "schedule_mode", json!("expression"));
    set(&mut v, "repeat_interval", json!("FREQ=DAILY;BYHOUR=99"));
    let e = s
        .scheduler_save("job", Some(&job), &v)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("ORA-27414") && !e.contains("deshabilitado"),
        "{e}"
    );
    assert_eq!(
        scalar(s, JOB_ROW).await,
        "|DBINE_TEST_SCHED_PROG|NAMED|SYSTEM.DBINE_TEST_SCHED_CAL|TRUE|"
    );
    // A call that fails after the disable (a queue that doesn't exist):
    // the job stays disabled, never enabled again with its schedule
    // nulled, and the error says so, the server's under it.
    let mut v = form(s, "job", &job).await;
    set(&mut v, "schedule_mode", json!("event"));
    set(&mut v, "event_queue", json!("SYSTEM.DBINE_TEST_SCHED_NO_Q"));
    let e = s
        .scheduler_save("job", Some(&job), &v)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("«SYSTEM.DBINE_TEST_SCHED_JOB» quedó deshabilitado")
            && e.contains("ORA-24085")
            && !e.contains("ORA-20991"),
        "{e}"
    );
    assert_eq!(
        scalar(s, "SELECT enabled || '|' || schedule_name FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB'").await,
        "FALSE|"
    );
    // Put back on its schedule and enabled.
    let mut v = form(s, "job", &job).await;
    set(&mut v, "schedule_mode", json!("named"));
    set(&mut v, "schedule", json!(cal));
    set(&mut v, "enabled", json!(true));
    save(s, "job", Some(&job), &v).await;
    assert_eq!(
        scalar(s, JOB_ROW).await,
        "|DBINE_TEST_SCHED_PROG|NAMED|SYSTEM.DBINE_TEST_SCHED_CAL|TRUE|"
    );
    unchanged(s, "job", &job).await;
    let mut v = form(s, "job", &job).await;
    set(&mut v, "schedule_mode", json!("window"));
    set(&mut v, "window", json!(win));
    set(&mut v, "stop_on_window_close", json!(true));
    save(s, "job", Some(&job), &v).await;
    assert_eq!(
        scalar(s, JOB_ROW).await,
        "|DBINE_TEST_SCHED_PROG|WINDOW|SYS.DBINE_TEST_SCHED_WIN|TRUE|"
    );
    unchanged(s, "job", &job).await;
    let mut v = form(s, "job", &job).await;
    set(&mut v, "schedule_mode", json!("event"));
    set(&mut v, "event_queue", json!("SYSTEM.DBINE_TEST_SCHED_Q"));
    set(&mut v, "event_condition", json!("tab.user_data.x = 3"));
    save(s, "job", Some(&job), &v).await;
    assert_eq!(
        scalar(s, "SELECT event_queue_owner || '.' || event_queue_name || '|' || event_condition FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB'").await,
        "SYSTEM.DBINE_TEST_SCHED_Q|tab.user_data.x = 3"
    );
    unchanged(s, "job", &job).await;
    let mut v = form(s, "job", &job).await;
    set(&mut v, "schedule_mode", json!("file_watcher"));
    set(&mut v, "file_watcher", json!(fw));
    set(&mut v, "enabled", json!(false));
    save(s, "job", Some(&job), &v).await;
    assert_eq!(
        scalar(s, "SELECT file_watcher_owner || '.' || file_watcher_name || '|' || enabled FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB'").await,
        "SYSTEM.DBINE_TEST_SCHED_FW|FALSE"
    );
    unchanged(s, "job", &job).await;
    let mut v = form(s, "job", &job).await;
    set(&mut v, "schedule_mode", json!("calendar"));
    set(&mut v, "freq", json!("HOURLY"));
    set(&mut v, "interval", json!(2));
    set(&mut v, "byhour", json!(""));
    set(&mut v, "job_type", json!("PLSQL_BLOCK"));
    set(&mut v, "plsql", json!("BEGIN NULL; END;"));
    list(&mut v, "arguments", json!([]));
    set(&mut v, "enabled", json!(true));
    set(&mut v, "max_runs", json!(null));
    save(s, "job", Some(&job), &v).await;
    assert_eq!(
        scalar(s, "SELECT job_type || '|' || program_name || '|' || repeat_interval || '|' || enabled || '|' || max_runs || '|' || file_watcher_name FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB'").await,
        "PLSQL_BLOCK||FREQ=HOURLY;INTERVAL=2|TRUE||"
    );
    unchanged(s, "job", &job).await;

    // A job whose action has a line SQL*Plus would read as the block's end.
    let tricky = "BEGIN\n  NULL;\n/\nEND;";
    let mut v = new_form(s, "job").await;
    set(&mut v, "name", json!("dbine_test_sched_job2"));
    set(&mut v, "plsql", json!(tricky));
    set(&mut v, "enabled", json!(false));
    let script = s.scheduler_script("job", None, &v).await.unwrap();
    assert_eq!(
        script.lines().filter(|l| l.trim() == "/").count(),
        1,
        "{script}"
    );
    let job2 = save(s, "job", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT job_action FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB2'").await,
        tricky
    );
    unchanged(s, "job", &job2).await;

    // A chain job, started at a step.
    let mut v = new_form(s, "job").await;
    set(&mut v, "name", json!("dbine_test_sched_chainjob"));
    set(&mut v, "job_type", json!("CHAIN"));
    set(&mut v, "chain", json!(chain));
    set(&mut v, "enabled", json!(false));
    let chainjob = save(s, "job", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT job_type || '|' || job_action FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_CHAINJOB'").await,
        "CHAIN|\"SYSTEM\".\"DBINE_TEST_SCHED_CHAIN\""
    );
    unchanged(s, "job", &chainjob).await;
    // The jobs list's ids open the designer.
    let items = s.scheduler_items("job").await.unwrap();
    assert!(items.items.iter().any(|i| i.id == job));
    let listed = s.jobs().await.unwrap();
    assert!(listed.jobs.iter().any(|j| j.id == job));
    (job, job2, chainjob)
}

async fn runs(s: &mut Box<dyn Session>, prog2: &str, chains_run: bool) {
    let mut v = new_form(s, "chain").await;
    set(&mut v, "name", json!("dbine_test_sched_rchain"));
    list(
        &mut v,
        "steps",
        json!([
            {"step_name": "S1", "step_type": "PROGRAM", "program": prog2},
            {"step_name": "S2", "step_type": "PROGRAM", "program": prog2}
        ]),
    );
    list(
        &mut v,
        "rules",
        json!([
            {"condition": "TRUE", "action": "START S1"},
            {"condition": "S1 COMPLETED", "action": "START S2"},
            {"condition": "S2 COMPLETED", "action": "END"}
        ]),
    );
    let chain = save(s, "chain", None, &v).await;
    let mut v = new_form(s, "job").await;
    set(&mut v, "name", json!("dbine_test_sched_rjob"));
    set(&mut v, "job_type", json!("CHAIN"));
    set(&mut v, "chain", json!(chain));
    set(&mut v, "enabled", json!(false));
    let rjob = save(s, "job", None, &v).await;
    let detail = s.job_detail(&rjob).await.unwrap();
    assert_eq!(
        detail
            .steps
            .iter()
            .map(|x| x.name.as_str())
            .collect::<Vec<_>>(),
        ["S1", "S2"]
    );
    assert!(s.run_job_from(&rjob, "NOPE").await.is_err());
    if chains_run {
        s.run_job_from(&rjob, "S2").await.unwrap();
        wait_for(
        s,
        "SELECT COUNT(*) FROM dba_scheduler_job_run_details WHERE job_name LIKE 'RUN\\_CHAIN$DBINE\\_%' ESCAPE '\\' AND job_subname = 'S2'",
        "the chain's run from S2",
    )
    .await;
        assert_eq!(
        scalar(s, "SELECT COUNT(*) FROM dba_scheduler_job_run_details WHERE job_name LIKE 'RUN\\_CHAIN$DBINE\\_%' ESCAPE '\\' AND job_subname = 'S1'").await,
        "0",
        "S1 doesn't run when the chain starts at S2"
    );
    } else {
        eprintln!("one job slave: a chain can't run; DBINE_TEST_ORACLE_SYSDBA_URL raises them");
    }

    // Run now, then clear the history.
    let mut v = new_form(s, "job").await;
    set(&mut v, "name", json!("dbine_test_sched_hjob"));
    set(&mut v, "plsql", json!("BEGIN NULL; END;"));
    set(&mut v, "enabled", json!(false));
    let hjob = save(s, "job", None, &v).await;
    if chains_run {
        s.run_job(&hjob).await.unwrap();
        wait_for(
            s,
            "SELECT COUNT(*) FROM dba_scheduler_job_run_details WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_HJOB'",
            "the job's run",
        )
        .await;
        s.clear_job_history(&hjob).await.unwrap();
        assert_eq!(
            scalar(s, "SELECT (SELECT COUNT(*) FROM dba_scheduler_job_run_details WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_HJOB') + (SELECT COUNT(*) FROM dba_scheduler_job_log WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_HJOB') FROM dual").await,
            "0"
        );
    } else {
        // No slave to run it: the call still has to work on a job without
        // runs.
        s.clear_job_history(&hjob).await.unwrap();
    }
    delete(s, "job", &hjob, "DROP_JOB").await;
    delete(s, "job", &rjob, "DROP_JOB").await;
    delete(s, "chain", &chain, "DROP_CHAIN").await;
}

async fn resources(
    s: &mut Box<dyn Session>,
    job: &str,
    job2: &str,
    chainjob: &str,
) -> (String, String) {
    let mut v = new_form(s, "resource").await;
    set(&mut v, "name", json!("dbine_test_sched_res"));
    set(&mut v, "units", json!(4));
    list(&mut v, "constraints", json!([{"object": job, "units": 2}]));
    let res = save(s, "resource", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT r.resource_units || '|' || r.status || '|' || c.object_name || ':' || c.units_used FROM dba_scheduler_resources r JOIN dba_scheduler_rsc_constraints c ON c.resource_owner = r.owner AND c.resource_name = r.resource_name WHERE r.owner = 'SYSTEM' AND r.resource_name = 'DBINE_TEST_SCHED_RES'").await,
        "4|ENFORCE_CONSTRAINTS|DBINE_TEST_SCHED_JOB:2"
    );
    unchanged(s, "resource", &res).await;
    let mut v = form(s, "resource", &res).await;
    set(&mut v, "units", json!(5));
    set(&mut v, "status", json!("IGNORE_CONSTRAINTS"));
    list(&mut v, "constraints", json!([{"object": job2, "units": 1}]));
    save(s, "resource", Some(&res), &v).await;
    assert_eq!(
        scalar(s, "SELECT r.resource_units || '|' || r.status || '|' || (SELECT LISTAGG(c.object_name, ',') FROM dba_scheduler_rsc_constraints c WHERE c.resource_owner = r.owner AND c.resource_name = r.resource_name) FROM dba_scheduler_resources r WHERE r.owner = 'SYSTEM' AND r.resource_name = 'DBINE_TEST_SCHED_RES'").await,
        "5|IGNORE_CONSTRAINTS|DBINE_TEST_SCHED_JOB2"
    );
    unchanged(s, "resource", &res).await;

    let mut v = new_form(s, "incompatibility").await;
    set(&mut v, "name", json!("dbine_test_sched_inc"));
    set(&mut v, "members", json!([job, chainjob]));
    let inc = save(s, "incompatibility", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT i.constraint_level || '|' || i.enabled || '|' || (SELECT LISTAGG(m.object_name, ',') WITHIN GROUP (ORDER BY m.object_name) FROM dba_scheduler_incompat_member m WHERE m.incompatibility_owner = i.owner AND m.incompatibility_name = i.incompatibility_name) FROM dba_scheduler_incompats i WHERE i.owner = 'SYSTEM' AND i.incompatibility_name = 'DBINE_TEST_SCHED_INC'").await,
        "JOB_LEVEL|YES|DBINE_TEST_SCHED_CHAINJOB,DBINE_TEST_SCHED_JOB"
    );
    unchanged(s, "incompatibility", &inc).await;
    let mut v = form(s, "incompatibility", &inc).await;
    set(&mut v, "members", json!([job, job2]));
    set(&mut v, "enabled", json!(false));
    set(&mut v, "comments", json!("dbine incompat"));
    save(s, "incompatibility", Some(&inc), &v).await;
    assert_eq!(
        scalar(s, "SELECT i.enabled || '|' || i.comments || '|' || (SELECT LISTAGG(m.object_name, ',') WITHIN GROUP (ORDER BY m.object_name) FROM dba_scheduler_incompat_member m WHERE m.incompatibility_owner = i.owner AND m.incompatibility_name = i.incompatibility_name) FROM dba_scheduler_incompats i WHERE i.owner = 'SYSTEM' AND i.incompatibility_name = 'DBINE_TEST_SCHED_INC'").await,
        "NO|dbine incompat|DBINE_TEST_SCHED_JOB,DBINE_TEST_SCHED_JOB2"
    );
    unchanged(s, "incompatibility", &inc).await;
    let mut v = form(s, "incompatibility", &inc).await;
    set(&mut v, "members", json!([job]));
    assert!(s
        .scheduler_save("incompatibility", Some(&inc), &v)
        .await
        .is_err());
    (res, inc)
}

async fn destinations(s: &mut Box<dyn Session>, cred: &str) -> String {
    // No agent registered on the test server: none to create a database
    // destination with, and no external destination.
    let f = s.scheduler_form("db_destination", None).await.unwrap();
    assert!(
        f.notes.iter().any(|n| n.contains("agentes")),
        "{:?}",
        f.notes
    );
    let mut v = values_of(&f);
    set(&mut v, "name", json!("dbine_test_sched_dd"));
    set(&mut v, "agent", json!("SYS.DBINE_TEST_SCHED_NOPE"));
    set(&mut v, "connect_info", json!("FREEPDB1"));
    assert!(s.scheduler_save("db_destination", None, &v).await.is_err());
    assert!(s
        .scheduler_form("external_destination", None)
        .await
        .is_err());

    let mut v = new_form(s, "destination_group").await;
    set(&mut v, "name", json!("dbine_test_sched_dgrp"));
    list(&mut v, "members", json!([{"destination": "LOCAL"}]));
    let dgrp = save(s, "destination_group", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT g.group_type || '|' || (SELECT LISTAGG(m.member_name, ',') FROM dba_scheduler_group_members m WHERE m.owner = g.owner AND m.group_name = g.group_name) FROM dba_scheduler_groups g WHERE g.owner = 'SYSTEM' AND g.group_name = 'DBINE_TEST_SCHED_DGRP'").await,
        "DB_DEST|LOCAL"
    );
    unchanged(s, "destination_group", &dgrp).await;
    let mut v = form(s, "destination_group", &dgrp).await;
    list(
        &mut v,
        "members",
        json!([{"destination": "LOCAL", "credential": cred}]),
    );
    assert!(
        s.scheduler_save("destination_group", Some(&dgrp), &v)
            .await
            .is_err(),
        "LOCAL takes no credential in a database destination group"
    );
    let mut v = form(s, "destination_group", &dgrp).await;
    set(&mut v, "comments", json!("dbine dests"));
    set(&mut v, "enabled", json!(false));
    save(s, "destination_group", Some(&dgrp), &v).await;
    assert_eq!(
        scalar(s, "SELECT enabled || '|' || comments FROM dba_scheduler_groups WHERE owner = 'SYSTEM' AND group_name = 'DBINE_TEST_SCHED_DGRP'").await,
        "FALSE|dbine dests"
    );
    unchanged(s, "destination_group", &dgrp).await;

    // Hosts: LOCAL, then LOCAL with a credential.
    let mut v = new_form(s, "destination_group").await;
    set(&mut v, "name", json!("dbine_test_sched_xgrp"));
    set(&mut v, "group_type", json!("EXTERNAL_DEST"));
    list(&mut v, "members", json!([{"destination": "LOCAL"}]));
    let xgrp = save(s, "destination_group", None, &v).await;
    unchanged(s, "destination_group", &xgrp).await;
    let mut v = form(s, "destination_group", &xgrp).await;
    list(
        &mut v,
        "members",
        json!([{"destination": "LOCAL", "credential": cred}]),
    );
    save(s, "destination_group", Some(&xgrp), &v).await;
    assert_eq!(
        scalar(s, "SELECT LISTAGG(member_name, ',') FROM dba_scheduler_group_members WHERE owner = 'SYSTEM' AND group_name = 'DBINE_TEST_SCHED_XGRP'").await,
        "\"SYSTEM\".\"DBINE_TEST_SCHED_CRED\"@LOCAL"
    );
    unchanged(s, "destination_group", &xgrp).await;
    delete(s, "destination_group", &xgrp, "DROP_GROUP").await;
    dgrp
}

async fn globals_and_logs(s: &mut Box<dyn Session>, job: &str) {
    let items = s.scheduler_items("global").await.unwrap();
    let id = items.items[0].id.clone();
    unchanged(s, "global", &id).await;
    let mut v = form(s, "global", &id).await;
    set(&mut v, "log_history", json!(31));
    set(&mut v, "email_server", json!("localhost:25"));
    set(&mut v, "email_sender", json!("dbine@example.com"));
    save(s, "global", Some(&id), &v).await;
    assert_eq!(
        scalar(s, "SELECT LISTAGG(attribute_name || '=' || value, ',') WITHIN GROUP (ORDER BY attribute_name) FROM dba_scheduler_global_attribute WHERE attribute_name IN ('LOG_HISTORY', 'EMAIL_SERVER', 'EMAIL_SENDER')").await,
        "EMAIL_SENDER=dbine@example.com,EMAIL_SERVER=localhost:25,LOG_HISTORY=31"
    );
    unchanged(s, "global", &id).await;

    // With a mail server, job notifications.
    let f = s.scheduler_form("job", Some(job)).await.unwrap();
    let notifications = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .find(|l| l.key == "notifications")
        .unwrap();
    assert!(!notifications.read_only);
    let mut v = values_of(&f);
    list(
        &mut v,
        "notifications",
        json!([{
            "recipients": "a@example.com, b@example.com",
            "events": ["JOB_FAILED", "JOB_BROKEN"],
            "subject": "%job_name%: %event_type%",
            "body": "dbine",
            "filter_condition": ""
        }]),
    );
    save(s, "job", Some(job), &v).await;
    assert_eq!(
        scalar(s, "SELECT COUNT(*) || '|' || MAX(subject) FROM dba_scheduler_notifications WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB'").await,
        "4|%job_name%: %event_type%"
    );
    unchanged(s, "job", job).await;
    let mut v = form(s, "job", job).await;
    list(&mut v, "notifications", json!([]));
    save(s, "job", Some(job), &v).await;
    assert_eq!(
        scalar(s, "SELECT COUNT(*) FROM dba_scheduler_notifications WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_JOB'").await,
        "0"
    );
    // Back as they were (the test's own restore checks it too).
    let mut v = form(s, "global", &id).await;
    set(&mut v, "log_history", json!(30));
    set(&mut v, "email_server", json!(""));
    set(&mut v, "email_sender", json!(""));
    save(s, "global", Some(&id), &v).await;

    let logs = s.scheduler_logs().await.unwrap();
    let keys: Vec<&str> = logs.iter().map(|t| t.key.as_str()).collect();
    assert_eq!(keys, ["job_log", "job_runs", "window_log", "window_runs"]);
    assert!(
        logs[0].rows.iter().any(|r| r[1]
            .as_str()
            .is_some_and(|j| j.contains("DBINE_TEST_SCHED"))),
        "the job log shows the test's jobs"
    );
}

async fn guards(cfg: &ConnectionConfig, s: &mut Box<dyn Session>, job: &str) {
    // Ids and kinds that aren't the driver's.
    for bad in [
        "SYSTEM.X'; DROP TABLE t; --",
        "\"SYSTEM\".\"A\"\"B\"",
        "system.dbine_test_sched_job",
        "DBMS_JOB:01",
        "",
    ] {
        assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad}");
        assert!(s.scheduler_delete("job", bad).await.is_err(), "{bad}");
    }
    assert!(s.scheduler_items("nope").await.is_err());
    // A form older than the job.
    let mut v = form(s, "job", job).await;
    exec(
        s,
        "BEGIN DBMS_SCHEDULER.SET_ATTRIBUTE('SYSTEM.DBINE_TEST_SCHED_JOB', 'comments', 'changed elsewhere'); END;",
    )
    .await;
    set(&mut v, "job_priority", json!(4));
    let e = s.scheduler_save("job", Some(job), &v).await.unwrap_err();
    assert!(e.to_string().contains("cambió en el servidor"), "{e}");
    // A read-only connection reads, never writes.
    let mut ro = ReadOnlySession::new(connect(cfg).await);
    let v = {
        let f = ro.scheduler_form("job", Some(job)).await.unwrap();
        values_of(&f)
    };
    assert!(ro.scheduler_script("job", Some(job), &v).await.is_ok());
    assert!(ro.scheduler_save("job", Some(job), &v).await.is_err());
    assert!(ro.scheduler_delete("job", job).await.is_err());
    assert!(ro.clear_job_history(job).await.is_err());

    // A login with CREATE JOB only.
    exec(
        s,
        &format!("CREATE USER {USER} IDENTIFIED BY \"{USER_PASSWORD}\" QUOTA UNLIMITED ON users"),
    )
    .await;
    exec(s, &format!("GRANT CREATE SESSION, CREATE JOB TO {USER}")).await;
    let mut ucfg = cfg.clone();
    ucfg.username = Some(USER.into());
    ucfg.password = Some(USER_PASSWORD.into());
    let mut u = connect(&ucfg).await;
    let info = u.scheduler_info().await.unwrap();
    let win = info.kinds.iter().find(|k| k.key == "window").unwrap();
    assert!(!win.can_create && !win.can_edit);
    let jobk = info.kinds.iter().find(|k| k.key == "job").unwrap();
    assert!(jobk.can_create);
    assert!(info.notes.iter().any(|n| n.contains("MANAGE SCHEDULER")));
    // Without MANAGE SCHEDULER: no history to clear, no forced stop, no
    // window to open; a program of its own still runs once.
    assert!(jobk.run_from_step && !jobk.clear_history);
    assert_eq!(keys(jobk), ["end_detached"]);
    assert!(win.actions.is_empty());
    let progk = info.kinds.iter().find(|k| k.key == "program").unwrap();
    assert_eq!(keys(progk), ["run"]);
    let w = item(&mut u, "window", "SYS.WEEKNIGHT_WINDOW").await;
    assert_eq!(
        (w.can_edit, w.can_delete, w.actions),
        (Some(false), Some(false), None)
    );
    let e = u
        .scheduler_action_script(
            "window",
            "SYS.WEEKNIGHT_WINDOW",
            "open_window",
            &Default::default(),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("MANAGE SCHEDULER"), "{e}");
    let f = u
        .scheduler_form("window", Some("SYS.WEEKNIGHT_WINDOW"))
        .await
        .unwrap();
    assert!(f.read_only);
    let v = values_of(&f);
    assert!(u
        .scheduler_save("window", Some("SYS.WEEKNIGHT_WINDOW"), &v)
        .await
        .is_err());
    // Its own job: created, changed, dropped.
    let f = u.scheduler_form("job", None).await.unwrap();
    let schema = f
        .sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|x| x.key == "owner")
        .unwrap();
    assert!(schema.read_only, "no CREATE ANY JOB: only its own schema");
    let mut v = values_of(&f);
    set(&mut v, "name", json!("dbine_test_sched_ujob"));
    set(&mut v, "plsql", json!("BEGIN NULL; END;"));
    set(&mut v, "enabled", json!(false));
    let ujob = save(&mut u, "job", None, &v).await;
    assert_eq!(ujob, format!("{USER}.DBINE_TEST_SCHED_UJOB"));
    unchanged(&mut u, "job", &ujob).await;
    // Clearing its history takes MANAGE SCHEDULER, owner or not.
    let e = u.clear_job_history(&ujob).await.unwrap_err().to_string();
    assert!(e.contains("MANAGE SCHEDULER"), "{e}");
    // Another schema's job: not without CREATE ANY JOB.
    let mut v = values_of(&u.scheduler_form("job", None).await.unwrap());
    set(&mut v, "owner", json!("SYSTEM"));
    set(&mut v, "name", json!("dbine_test_sched_ujob2"));
    set(&mut v, "plsql", json!("BEGIN NULL; END;"));
    assert!(u.scheduler_save("job", None, &v).await.is_err());
    delete(&mut u, "job", &ujob, "DROP_JOB").await;
}

/// One object as the tree lists it.
async fn item(s: &mut Box<dyn Session>, kind: &str, id: &str) -> SchedulerItem {
    s.scheduler_items(kind)
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id == id)
        .unwrap_or_else(|| panic!("{kind} {id} not listed"))
}

/// A script on one line: a long call is written one argument a line.
fn flat(script: &str) -> String {
    script
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("( ", "(")
}

fn actions_of(i: &SchedulerItem) -> Vec<String> {
    i.actions.clone().expect("the item says its actions")
}

fn keys(k: &SchedulerKind) -> Vec<&str> {
    k.actions.iter().map(|a| a.key.as_str()).collect()
}

const WACT: &str = "SELECT LISTAGG(window_name || '=' || active, ',') WITHIN GROUP (ORDER BY window_name) FROM dba_scheduler_windows WHERE window_name LIKE 'DBINE\\_TEST\\_SCHED\\_WACT%' ESCAPE '\\'";

/// The kinds' actions: a window opened for its duration and closed, a
/// program and a chain run once, a running job stopped by force and a
/// detached run ended; each one's script first.
async fn actions(cfg: &ConnectionConfig, s: &mut Box<dyn Session>, prog2: &str, chains_run: bool) {
    let info = s.scheduler_info().await.unwrap();
    let kind = |k: &str| info.kinds.iter().find(|x| x.key == k).unwrap().clone();
    let job = kind("job");
    assert!(job.run_from_step && job.clear_history);
    assert_eq!(keys(&job), ["force_stop", "end_detached"]);
    assert_eq!(keys(&kind("window")), ["open_window", "close_window"]);
    assert_eq!(keys(&kind("program")), ["run"]);
    assert_eq!(keys(&kind("chain")), ["run"]);
    assert!(kind("schedule").actions.is_empty() && !kind("program").run_from_step);
    // Not the kind's, or no action at all.
    for (k, a) in [
        ("program", "open_window"),
        ("program", "run'; --"),
        ("window", "run"),
    ] {
        assert!(
            s.scheduler_action_script(k, prog2, a, &Default::default())
                .await
                .is_err(),
            "{k} {a}"
        );
    }
    assert!(s
        .scheduler_action("schedule", "SYSTEM.X", "run", &Default::default())
        .await
        .is_err());

    // Windows: opened now for their duration, closed before their time.
    let mut v = new_form(s, "window").await;
    set(&mut v, "name", json!("dbine_test_sched_wact"));
    set(&mut v, "byhour", json!("1"));
    set(&mut v, "duration", json!(600));
    let w1 = save(s, "window", None, &v).await;
    set(&mut v, "name", json!("dbine_test_sched_wact2"));
    let w2 = save(s, "window", None, &v).await;
    let i = item(s, "window", &w1).await;
    assert_eq!(actions_of(&i), ["open_window"]);
    assert_eq!((i.system, i.can_edit, i.can_delete), (false, None, None));
    // A read-only connection shows the script, doesn't run it.
    let mut ro = ReadOnlySession::new(connect(cfg).await);
    assert!(ro
        .scheduler_action_script("window", &w1, "open_window", &Default::default())
        .await
        .is_ok());
    assert!(ro
        .scheduler_action("window", &w1, "open_window", &Default::default())
        .await
        .is_err());
    assert_eq!(
        scalar(s, WACT).await,
        "DBINE_TEST_SCHED_WACT=FALSE,DBINE_TEST_SCHED_WACT2=FALSE"
    );
    let open = scalar(
        s,
        "SELECT COUNT(*) FROM dba_scheduler_windows WHERE active = 'TRUE'",
    )
    .await;
    if open == "0" {
        let script = s
            .scheduler_action_script("window", &w1, "open_window", &Default::default())
            .await
            .unwrap();
        assert!(
            script.contains("OPEN_WINDOW(window_name => '\"SYS\".\"DBINE_TEST_SCHED_WACT\"', duration => NULL, force => FALSE);")
                && script.contains("00:10:00"),
            "{script}"
        );
        s.scheduler_action("window", &w1, "open_window", &Default::default())
            .await
            .unwrap();
        assert_eq!(
            scalar(s, WACT).await,
            "DBINE_TEST_SCHED_WACT=TRUE,DBINE_TEST_SCHED_WACT2=FALSE"
        );
        assert_eq!(actions_of(&item(s, "window", &w1).await), ["close_window"]);
        let e = s
            .scheduler_action_script("window", &w1, "open_window", &Default::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("ya está abierta"), "{e}");
        // Only one is open at a time: opening another closes it.
        let script = s
            .scheduler_action_script("window", &w2, "open_window", &Default::default())
            .await
            .unwrap();
        assert!(
            script.contains("force => TRUE") && script.contains("SYS.DBINE_TEST_SCHED_WACT "),
            "{script}"
        );
        s.scheduler_action("window", &w2, "open_window", &Default::default())
            .await
            .unwrap();
        assert_eq!(
            scalar(s, WACT).await,
            "DBINE_TEST_SCHED_WACT=FALSE,DBINE_TEST_SCHED_WACT2=TRUE"
        );
        let script = s
            .scheduler_action_script("window", &w2, "close_window", &Default::default())
            .await
            .unwrap();
        assert!(
            script.contains("CLOSE_WINDOW(window_name => '\"SYS\".\"DBINE_TEST_SCHED_WACT2\"');"),
            "{script}"
        );
        s.scheduler_action("window", &w2, "close_window", &Default::default())
            .await
            .unwrap();
        assert_eq!(
            scalar(s, WACT).await,
            "DBINE_TEST_SCHED_WACT=FALSE,DBINE_TEST_SCHED_WACT2=FALSE"
        );
        let e = s
            .scheduler_action_script("window", &w2, "close_window", &Default::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no está abierta"), "{e}");
    } else {
        eprintln!("a window of the server's is open: opening the test's would close it; skipped");
    }
    // Oracle doesn't open a disabled window.
    let mut v = form(s, "window", &w2).await;
    set(&mut v, "enabled", json!(false));
    save(s, "window", Some(&w2), &v).await;
    assert!(actions_of(&item(s, "window", &w2).await).is_empty());
    let e = s
        .scheduler_action_script("window", &w2, "open_window", &Default::default())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("deshabilitada"), "{e}");
    delete(s, "window", &w1, "DROP_WINDOW").await;
    delete(s, "window", &w2, "DROP_WINDOW").await;

    // A program run once, in a run-once job of its own; one whose argument
    // has no value isn't.
    assert_eq!(actions_of(&item(s, "program", prog2).await), ["run"]);
    let mut v = new_form(s, "program").await;
    set(&mut v, "name", json!("dbine_test_sched_aprog"));
    set(&mut v, "program_type", json!("STORED_PROCEDURE"));
    set(&mut v, "procedure", json!("SYSTEM.DBINE_TEST_SCHED_PROC"));
    list(
        &mut v,
        "arguments",
        json!([
            {"name": "p_n", "metadata": "VALUE", "arg_type": "NUMBER"},
            {"name": "p_job", "metadata": "JOB_NAME"}
        ]),
    );
    let aprog = save(s, "program", None, &v).await;
    assert!(actions_of(&item(s, "program", &aprog).await).is_empty());
    let e = s
        .scheduler_action_script("program", &aprog, "run", &Default::default())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("argumentos sin valor"), "{e}");
    delete(s, "program", &aprog, "DROP_PROGRAM").await;
    let script = s
        .scheduler_action_script("program", prog2, "run", &Default::default())
        .await
        .unwrap();
    assert!(
        script.contains("job_name => DBMS_SCHEDULER.GENERATE_JOB_NAME('RUN_PROG$DBINE_TE')")
            && script.contains("program_name => '\"SYSTEM\".\"DBINE_TEST_SCHED_PROG2\"'")
            && script.contains("enabled => TRUE")
            && script.contains("auto_drop => TRUE"),
        "{script}"
    );
    // A chain run once, from its start.
    let mut v = new_form(s, "chain").await;
    set(&mut v, "name", json!("dbine_test_sched_achain"));
    list(
        &mut v,
        "steps",
        json!([{"step_name": "A1", "step_type": "PROGRAM", "program": prog2}]),
    );
    list(
        &mut v,
        "rules",
        json!([
            {"condition": "TRUE", "action": "START A1"},
            {"condition": "A1 COMPLETED", "action": "END"}
        ]),
    );
    let achain = save(s, "chain", None, &v).await;
    assert_eq!(actions_of(&item(s, "chain", &achain).await), ["run"]);
    let script = flat(
        &s.scheduler_action_script("chain", &achain, "run", &Default::default())
            .await
            .unwrap(),
    );
    assert!(
        script.contains("RUN_CHAIN(chain_name => '\"SYSTEM\".\"DBINE_TEST_SCHED_ACHAIN\"', start_steps => NULL, job_name => NULL);"),
        "{script}"
    );
    if chains_run {
        s.scheduler_action("program", prog2, "run", &Default::default())
            .await
            .unwrap();
        wait_for(
            s,
            "SELECT COUNT(*) FROM dba_scheduler_job_run_details WHERE owner = 'SYSTEM' AND job_name LIKE 'RUN\\_PROG$DBINE\\_TE%' ESCAPE '\\' AND status = 'SUCCEEDED'",
            "the program's run",
        )
        .await;
        s.scheduler_action("chain", &achain, "run", &Default::default())
            .await
            .unwrap();
        wait_for(
            s,
            "SELECT COUNT(*) FROM dba_scheduler_job_run_details WHERE job_name LIKE 'RUN\\_CHAIN$DBINE\\_%' ESCAPE '\\' AND job_subname = 'A1'",
            "the chain's run",
        )
        .await;
        // Its run-once job goes when the chain ends.
        wait_for(
            s,
            "SELECT CASE WHEN COUNT(*) = 0 THEN 1 ELSE 0 END FROM dba_scheduler_jobs WHERE job_name LIKE 'RUN\\_%$DBINE\\_%' ESCAPE '\\'",
            "the run-once jobs dropped",
        )
        .await;
    }
    // Disabled, it doesn't run.
    let mut v = form(s, "chain", &achain).await;
    set(&mut v, "enabled", json!(false));
    save(s, "chain", Some(&achain), &v).await;
    assert!(actions_of(&item(s, "chain", &achain).await).is_empty());
    let e = s
        .scheduler_action_script("chain", &achain, "run", &Default::default())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("deshabilitada"), "{e}");
    delete(s, "chain", &achain, "DROP_CHAIN").await;

    if !chains_run {
        eprintln!("one job slave: stopping a running job and ending a detached run skipped");
        return;
    }
    // A job that runs a while, stopped by force.
    let mut v = new_form(s, "job").await;
    set(&mut v, "name", json!("dbine_test_sched_sjob"));
    set(
        &mut v,
        "plsql",
        json!("BEGIN DBMS_SESSION.SLEEP(120); END;"),
    );
    set(&mut v, "enabled", json!(false));
    let sjob = save(s, "job", None, &v).await;
    assert!(actions_of(&item(s, "job", &sjob).await).is_empty());
    s.run_job(&sjob).await.unwrap();
    wait_for(
        s,
        "SELECT COUNT(*) FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_SJOB' AND state = 'RUNNING'",
        "the job running",
    )
    .await;
    assert_eq!(actions_of(&item(s, "job", &sjob).await), ["force_stop"]);
    let script = s
        .scheduler_action_script("job", &sjob, "force_stop", &Default::default())
        .await
        .unwrap();
    assert!(
        script.contains(
            "STOP_JOB(job_name => '\"SYSTEM\".\"DBINE_TEST_SCHED_SJOB\"', force => TRUE);"
        ),
        "{script}"
    );
    s.scheduler_action("job", &sjob, "force_stop", &Default::default())
        .await
        .unwrap();
    assert_ne!(
        scalar(s, "SELECT state FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_SJOB'").await,
        "RUNNING"
    );
    let e = s
        .scheduler_action_script("job", &sjob, "force_stop", &Default::default())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no se está ejecutando"), "{e}");
    delete(s, "job", &sjob, "DROP_JOB").await;

    // A job of SYS's running (as Oracle's own maintenance jobs are): this
    // login (SYSTEM) can't change it, CREATE ANY JOB doesn't reach it, yet
    // it stops it by force, since Oracle asks only for MANAGE SCHEDULER.
    if let Some(sys) = sys_pdb_config(cfg) {
        let mut sys = connect(&sys).await;
        exec(
            &mut sys,
            "BEGIN
  DBMS_SCHEDULER.CREATE_JOB(job_name => '\"SYS\".\"DBINE_TEST_SCHED_SYSJOB\"', job_type => 'PLSQL_BLOCK',
    job_action => 'BEGIN DBMS_SESSION.SLEEP(120); END;', enabled => FALSE, auto_drop => FALSE);
  DBMS_SCHEDULER.RUN_JOB('\"SYS\".\"DBINE_TEST_SCHED_SYSJOB\"', use_current_session => FALSE);
END;",
        )
        .await;
        wait_for(
            s,
            "SELECT COUNT(*) FROM dba_scheduler_jobs WHERE owner = 'SYS' AND job_name = 'DBINE_TEST_SCHED_SYSJOB' AND state = 'RUNNING'",
            "SYS's job running",
        )
        .await;
        let sys_job = item(s, "job", "SYS.DBINE_TEST_SCHED_SYSJOB").await;
        assert!(sys_job.system);
        assert_eq!(
            (sys_job.can_edit, sys_job.can_delete),
            (Some(false), Some(false))
        );
        assert_eq!(actions_of(&sys_job), ["force_stop"]);
        let script = s
            .scheduler_action_script("job", &sys_job.id, "force_stop", &Default::default())
            .await
            .unwrap();
        assert!(
            script.contains(
                "STOP_JOB(job_name => '\"SYS\".\"DBINE_TEST_SCHED_SYSJOB\"', force => TRUE);"
            ),
            "{script}"
        );
        s.scheduler_action("job", &sys_job.id, "force_stop", &Default::default())
            .await
            .unwrap();
        assert_ne!(
            scalar(s, "SELECT state FROM dba_scheduler_jobs WHERE owner = 'SYS' AND job_name = 'DBINE_TEST_SCHED_SYSJOB'").await,
            "RUNNING"
        );
        // Ending its run is the owner's business, as "Detener" is.
        assert!(s
            .scheduler_action_script("job", &sys_job.id, "end_detached", &Default::default())
            .await
            .is_err());
        exec(&mut sys, SYS_CLEANUP).await;
    }

    // A detached program's run: running until it's ended.
    let mut v = new_form(s, "program").await;
    set(&mut v, "name", json!("dbine_test_sched_dprog"));
    set(&mut v, "plsql", json!("BEGIN NULL; END;"));
    set(&mut v, "detached", json!(true));
    let dprog = save(s, "program", None, &v).await;
    let mut v = new_form(s, "job").await;
    set(&mut v, "name", json!("dbine_test_sched_djob"));
    set(&mut v, "job_type", json!("PROGRAM"));
    set(&mut v, "program", json!(dprog));
    set(&mut v, "enabled", json!(false));
    let djob = save(s, "job", None, &v).await;
    s.run_job(&djob).await.unwrap();
    wait_for(
        s,
        "SELECT COUNT(*) FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_DJOB' AND state = 'RUNNING'",
        "the detached job running",
    )
    .await;
    assert_eq!(
        actions_of(&item(s, "job", &djob).await),
        ["force_stop", "end_detached"]
    );
    let script = s
        .scheduler_action_script("job", &djob, "end_detached", &Default::default())
        .await
        .unwrap();
    assert!(
        script.contains("END_DETACHED_JOB_RUN(job_name => '\"SYSTEM\".\"DBINE_TEST_SCHED_DJOB\"', error_number => 0);"),
        "{script}"
    );
    s.scheduler_action("job", &djob, "end_detached", &Default::default())
        .await
        .unwrap();
    wait_for(
        s,
        "SELECT COUNT(*) FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_DJOB' AND state <> 'RUNNING'",
        "the detached run ended",
    )
    .await;
    let e = s
        .scheduler_action_script("job", &djob, "end_detached", &Default::default())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("desacoplado"), "{e}");
    delete(s, "job", &djob, "DROP_JOB").await;
    delete(s, "program", &dprog, "DROP_PROGRAM").await;
}

/// What each object allows: Oracle's own are shown, not changed nor
/// dropped (its maintenance windows and their group change, as Oracle
/// documents, but aren't dropped); the test's own, as their kind says.
async fn abilities(s: &mut Box<dyn Session>, job: &str, class: &str) {
    let own = item(s, "job", job).await;
    assert_eq!((own.can_edit, own.can_delete), (None, None));
    let jobs = s.scheduler_items("job").await.unwrap();
    let sys_job = jobs
        .items
        .iter()
        .find(|i| i.id.starts_with("SYS."))
        .expect("SYS's jobs");
    assert!(sys_job.system);
    assert_eq!(
        (sys_job.can_edit, sys_job.can_delete),
        (Some(false), Some(false))
    );
    let f = s.scheduler_form("job", Some(&sys_job.id)).await.unwrap();
    assert!(
        f.read_only && f.notes.iter().any(|n| n.contains("de Oracle")),
        "{:?}",
        f.notes
    );
    assert!(s.scheduler_delete_script("job", &sys_job.id).await.is_err());
    // A program and a schedule of SYS's: the same, and not run.
    for kind in ["program", "schedule"] {
        let items = s.scheduler_items(kind).await.unwrap();
        let sys = items
            .items
            .iter()
            .find(|i| i.id.starts_with("SYS."))
            .unwrap_or_else(|| panic!("SYS's {kind}s"));
        assert_eq!(
            (sys.can_edit, sys.can_delete),
            (Some(false), Some(false)),
            "{kind}"
        );
        if kind == "program" {
            assert_eq!(sys.actions, Some(Vec::new()));
            assert!(s
                .scheduler_action_script(kind, &sys.id, "run", &Default::default())
                .await
                .is_err());
        }
        let f = s.scheduler_form(kind, Some(&sys.id)).await.unwrap();
        assert!(f.read_only, "{kind} {}", sys.id);
        let v = values_of(&f);
        assert!(s.scheduler_save(kind, Some(&sys.id), &v).await.is_err());
    }
    // Job classes Oracle installed: locked; the test's: as the kind.
    let dc = item(s, "job_class", "SYS.DEFAULT_JOB_CLASS").await;
    assert!(dc.system);
    assert_eq!((dc.can_edit, dc.can_delete), (Some(false), Some(false)));
    assert!(
        s.scheduler_form("job_class", Some("SYS.DEFAULT_JOB_CLASS"))
            .await
            .unwrap()
            .read_only
    );
    assert!(s
        .scheduler_delete_script("job_class", "SYS.DEFAULT_JOB_CLASS")
        .await
        .is_err());
    let tc = item(s, "job_class", class).await;
    assert_eq!((tc.system, tc.can_edit, tc.can_delete), (false, None, None));
    // Maintenance windows and their group: changed, not dropped; Oracle's
    // internal groups: neither.
    let mw = item(s, "window", "SYS.MONDAY_WINDOW").await;
    assert_eq!(
        (mw.system, mw.can_edit, mw.can_delete),
        (true, None, Some(false))
    );
    let f = s
        .scheduler_form("window", Some("SYS.MONDAY_WINDOW"))
        .await
        .unwrap();
    assert!(!f.read_only, "{:?}", f.notes);
    unchanged(s, "window", "SYS.MONDAY_WINDOW").await;
    let e = s
        .scheduler_delete_script("window", "SYS.MONDAY_WINDOW")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no borrar"), "{e}");
    let g = item(s, "window_group", "SYS.MAINTENANCE_WINDOW_GROUP").await;
    assert_eq!(
        (g.system, g.can_edit, g.can_delete),
        (true, None, Some(false))
    );
    let groups = s.scheduler_items("window_group").await.unwrap();
    if let Some(at) = groups
        .items
        .iter()
        .find(|i| i.id.starts_with("SYS.\"ORA$") || i.id.starts_with("SYS.ORA$"))
    {
        assert_eq!(
            (at.can_edit, at.can_delete),
            (Some(false), Some(false)),
            "{}",
            at.id
        );
        assert!(
            s.scheduler_form("window_group", Some(&at.id))
                .await
                .unwrap()
                .read_only
        );
    }
}

fn field(f: &ObjectForm, key: &str) -> dbine_driver::FormField {
    f.sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|x| x.key == key)
        .unwrap_or_else(|| panic!("no field {key}"))
        .clone()
}

fn cond(field: &str, values: &[&str]) -> Option<(String, Vec<String>)> {
    Some((
        field.to_string(),
        values.iter().map(|v| v.to_string()).collect(),
    ))
}

/// The fields, lists and tabs that show only for some modes and types.
async fn conditions(s: &mut Box<dyn Session>) {
    let f = s.scheduler_form("job", None).await.unwrap();
    let args = f
        .sections
        .iter()
        .flat_map(|x| x.lists.iter())
        .find(|l| l.key == "arguments")
        .unwrap();
    assert_eq!(
        args.when,
        cond(
            "job_type",
            &[
                "STORED_PROCEDURE",
                "EXECUTABLE",
                "EXTERNAL_SCRIPT",
                "PROGRAM"
            ]
        )
    );
    assert_eq!(
        field(&f, "fail_on_script_error").when,
        cond(
            "job_type",
            &["EXTERNAL_SCRIPT", "SQL_SCRIPT", "BACKUP_SCRIPT", "PROGRAM"]
        )
    );
    assert_eq!(
        field(&f, "byweekno").when,
        cond("schedule_mode", &["calendar"])
    );
    assert_eq!(
        field(&f, "byweekno").when_all,
        [("freq".to_string(), vec!["YEARLY".to_string()])]
    );
    assert_eq!(
        field(&f, "bysetpos").when_all,
        [(
            "freq".to_string(),
            vec!["YEARLY".to_string(), "MONTHLY".to_string()]
        )]
    );
    // 19c and later: no DBMS_JOB choice, so every tab shows.
    assert!(f.sections.iter().all(|x| x.when.is_none()));
    assert!(f
        .sections
        .iter()
        .flat_map(|x| x.fields.iter())
        .all(|x| x.key != "job_api"));
    let f = s.scheduler_form("program", None).await.unwrap();
    let tab = f.sections.iter().find(|x| x.key == "arguments").unwrap();
    assert_eq!(
        tab.when,
        cond(
            "program_type",
            &["STORED_PROCEDURE", "EXECUTABLE", "EXTERNAL_SCRIPT"]
        )
    );
    let f = s.scheduler_form("chain", None).await.unwrap();
    let steps = f
        .sections
        .iter()
        .flat_map(|x| x.lists.iter())
        .find(|l| l.key == "steps")
        .unwrap();
    for k in ["credential", "destination"] {
        let x = steps.fields.iter().find(|x| x.key == k).unwrap();
        assert_eq!(x.when, cond("step_type", &["PROGRAM"]), "{k}");
    }
    // A job of a type without arguments: rows the hidden list still holds
    // don't apply.
    let mut v = new_form(s, "job").await;
    set(&mut v, "name", json!("dbine_test_sched_cjob"));
    set(&mut v, "plsql", json!("BEGIN NULL; END;"));
    set(&mut v, "enabled", json!(false));
    set(&mut v, "schedule_mode", json!("calendar"));
    set(&mut v, "freq", json!("DAILY"));
    set(&mut v, "byhour", json!("2"));
    // Hidden under DAILY: Oracle would refuse them (ORA-27417, ORA-27424).
    set(&mut v, "byweekno", json!("10"));
    set(&mut v, "bysetpos", json!("-1"));
    list(&mut v, "arguments", json!([{"value": "1"}]));
    let cjob = save(s, "job", None, &v).await;
    assert_eq!(
        scalar(s, "SELECT repeat_interval || '|' || number_of_arguments FROM dba_scheduler_jobs WHERE owner = 'SYSTEM' AND job_name = 'DBINE_TEST_SCHED_CJOB'").await,
        "FREQ=DAILY;BYHOUR=2|0"
    );
    unchanged(s, "job", &cjob).await;
    delete(s, "job", &cjob, "DROP_JOB").await;
}

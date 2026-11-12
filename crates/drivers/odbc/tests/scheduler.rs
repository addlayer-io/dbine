//! The scheduler designer against a real server, through the engine's own
//! ODBC driver and the engine's preset: each test creates every kind the
//! engine's scheduler has (objects named `dbine_test_sched_*`; on IBM i,
//! whose names have 10 characters, `DBTSCHED01`), reads it back in its
//! form, changes it, checks the change on the server and deletes it. What
//! it created is removed again at the end, also when a step failed, before
//! anything is asserted.
//!
//! There are no containers for these engines here: the tests run only
//! with one variable per engine, with the connection fields as
//! `key=value;…` (as in `tests/jobs.rs`):
//!
//! ```sh
//! DBINE_TEST_ODBC_SCHED_DB2='host=…;port=50000;database=testdb;user=db2inst1;password=…;odbc_driver=IBM DB2 ODBC DRIVER' \
//! DBINE_TEST_ODBC_SCHED_DB2ZOS='host=…;port=…;database=…;user=…;password=…;odbc_driver=IBM DB2 ODBC DRIVER' \
//! DBINE_TEST_ODBC_SCHED_DB2I='host=…;user=…;password=…;odbc_driver=IBM i Access ODBC Driver' \
//! DBINE_TEST_ODBC_SCHED_INFORMIX='host=127.0.0.1;port=9088;database=sysadmin;user=informix;password=in4mix;odbc_driver=IBM INFORMIX ODBC DRIVER;extra=SERVER=informix' \
//! DBINE_TEST_ODBC_SCHED_HIVE='host=127.0.0.1;port=10000;user=hive;odbc_driver=Cloudera ODBC Driver for Apache Hive' \
//! DBINE_TEST_ODBC_SCHED_VERTICA='host=127.0.0.1;port=5433;database=vmart;user=dbadmin;password=…;odbc_driver=Vertica' \
//! DBINE_TEST_ODBC_SCHED_ASE='host=…;port=5000;database=master;user=sa;password=…;odbc_driver=Adaptive Server Enterprise' \
//! DBINE_TEST_ODBC_SCHED_SQLA='host=…;port=2638;database=demo;user=dba;password=…;odbc_driver=SQL Anywhere 17' \
//! DBINE_TEST_ODBC_SCHED_IRIS='host=…;port=1972;database=%SYS;user=_SYSTEM;password=…;odbc_driver=InterSystems IRIS ODBC35' \
//! DBINE_TEST_ODBC_SCHED_DAMENG='host=…;port=5236;user=SYSDBA;password=…;odbc_driver=DM8 ODBC DRIVER' \
//! DBINE_TEST_ODBC_SCHED_ALTIBASE='host=…;port=20300;user=SYS;password=…;odbc_driver=ALTIBASE_HDB_ODBC_64bit' \
//! DBINE_TEST_ODBC_SCHED_VIRTUOSO='host=…;port=1111;user=dba;password=…;odbc_driver=Virtuoso' \
//!   cargo test -p dbine-driver-odbc --test scheduler -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, FormValues, ObjectForm, QueryOutcome, Session};
use serde_json::{json, Value};
use std::collections::BTreeMap;

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
    assert!(d.capabilities().scheduler, "{} has the scheduler", c.driver);
    d.connect(c, None).await.expect("connect")
}

/// Runs SQL, ignoring the outcome (cleanup).
async fn quietly(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 10, &mut out).await {
        println!("cleanup: {e}");
    }
}

/// The form's values with `over` on top, and its lists' rows.
fn values_of(f: &ObjectForm, over: &Value) -> FormValues {
    let mut values = f.values.clone();
    let mut lists: BTreeMap<String, Vec<BTreeMap<String, Value>>> = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .map(|l| (l.key.clone(), l.rows.clone()))
        .collect();
    for (k, v) in over.as_object().expect("an object") {
        if let Some(rows) = k.strip_prefix("list:") {
            lists.insert(
                rows.to_string(),
                serde_json::from_value(v.clone()).expect("rows"),
            );
        } else {
            values.insert(k.clone(), v.clone());
        }
    }
    FormValues { values, lists }
}

fn check(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what.into())
    }
}

/// Create an object of `kind` with `create`, read it back, change it with
/// `change` and check `changed` (a field and its new value) in its form;
/// then delete it and check it's gone. Returns its last id.
async fn round_trip(
    s: &mut Box<dyn Session>,
    kind: &str,
    create: Value,
    change: Value,
    changed: (&str, Value),
) -> Result<String, String> {
    let info = s.scheduler_info().await.map_err(|e| e.to_string())?;
    println!("{info:?}");
    check(
        info.kinds.iter().any(|k| k.key == kind && k.can_create),
        format!("{kind} can be created"),
    )?;
    let new = s
        .scheduler_form(kind, None)
        .await
        .map_err(|e| e.to_string())?;
    let v = values_of(&new, &create);
    let script = s
        .scheduler_script(kind, None, &v)
        .await
        .map_err(|e| e.to_string())?;
    println!("{script}");
    let id = s
        .scheduler_save(kind, None, &v)
        .await
        .map_err(|e| format!("{e}\n---\n{script}"))?
        .ok_or("the save gave no id")?;
    let listed = s.scheduler_items(kind).await.map_err(|e| e.to_string())?;
    check(
        listed.items.iter().any(|i| i.id == id),
        format!("{id} isn't listed: {listed:?}"),
    )?;
    let form = s
        .scheduler_form(kind, Some(&id))
        .await
        .map_err(|e| e.to_string())?;
    let unchanged = s
        .scheduler_script(kind, Some(&id), &values_of(&form, &json!({})))
        .await
        .map_err(|e| e.to_string())?;
    check(
        unchanged.contains("no hay cambios"),
        format!("read back as saved: {unchanged}"),
    )?;
    let v = values_of(&form, &change);
    let script = s
        .scheduler_script(kind, Some(&id), &v)
        .await
        .map_err(|e| e.to_string())?;
    println!("{script}");
    let id = s
        .scheduler_save(kind, Some(&id), &v)
        .await
        .map_err(|e| format!("{e}\n---\n{script}"))?
        .unwrap_or(id);
    let form = s
        .scheduler_form(kind, Some(&id))
        .await
        .map_err(|e| e.to_string())?;
    check(
        form.values.get(changed.0) == Some(&changed.1),
        format!("{} is {:?}", changed.0, form.values.get(changed.0)),
    )?;
    println!(
        "{}",
        s.scheduler_delete_script(kind, &id)
            .await
            .map_err(|e| e.to_string())?
    );
    s.scheduler_delete(kind, &id)
        .await
        .map_err(|e| e.to_string())?;
    let listed = s.scheduler_items(kind).await.map_err(|e| e.to_string())?;
    check(
        !listed.items.iter().any(|i| i.id == id),
        format!("{id} is still listed"),
    )?;
    Ok(id)
}

#[tokio::test]
#[ignore = "needs a Db2 LUW server (DBINE_TEST_ODBC_SCHED_DB2)"]
async fn db2_tasks() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_DB2", "db2") else {
        return;
    };
    let mut s = open(&c).await;
    let r = round_trip(
        &mut s,
        "job",
        json!({
            "name": "dbine_test_sched_db2", "procedure_schema": "SYSPROC",
            "procedure_name": "ADMIN_CMD", "procedure_input": "VALUES ('RUNSTATS ON TABLE SYSIBM.SYSTABLES')",
            "schedule": "0 3 * * *", "max_invocations": 1, "remarks": "it's a test"
        }),
        json!({"schedule": "30 4 * * *"}),
        ("schedule", json!("30 4 * * *")),
    )
    .await;
    quietly(
        &mut s,
        "CALL SYSPROC.ADMIN_TASK_REMOVE('dbine_test_sched_db2', NULL)",
    )
    .await;
    r.unwrap();
}

#[tokio::test]
#[ignore = "needs a Db2 for z/OS subsystem (DBINE_TEST_ODBC_SCHED_DB2ZOS)"]
async fn db2zos_tasks() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_DB2ZOS", "db2zos") else {
        return;
    };
    let mut s = open(&c).await;
    let r = round_trip(
        &mut s,
        "job",
        json!({
            "task_name": "DBINE_TEST_SCHED_ZOS", "action": "procedure", "procedure_schema": "SYSPROC",
            "procedure_name": "ADMIN_INFO_SYSPARM", "trigger": "interval", "interval": 1440,
            "max_invocations": 1
        }),
        // A new name: the task is added again (its name bound as INOUT)
        // and the old one removed after.
        json!({"interval": 720, "task_name": "DBINE_TEST_SCHED_ZOS2"}),
        ("interval", json!(720)),
    )
    .await;
    // The procedure answers in output parameters: removed through DBine.
    for name in ["DBINE_TEST_SCHED_ZOS", "DBINE_TEST_SCHED_ZOS2"] {
        let _ = s.scheduler_delete("job", name).await;
    }
    r.unwrap();
}

#[tokio::test]
#[ignore = "needs an IBM i system (DBINE_TEST_ODBC_SCHED_DB2I)"]
async fn db2i_entries() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_DB2I", "db2i") else {
        return;
    };
    let mut s = open(&c).await;
    let r = round_trip(
        &mut s,
        "job",
        json!({
            "name": "DBTSCHED01", "command": "DLYJOB DLY(1)", "frequency": "*WEEKLY",
            "date_mode": "*NONE", "days": ["*SUN"], "time": "03:00:00", "text": "it's a test",
            "enabled": false
        }),
        json!({"days": ["*SUN", "*SAT"], "enabled": true}),
        ("days", json!(["*SAT", "*SUN"])),
    )
    .await;
    quietly(
        &mut s,
        "CALL QSYS2.QCMDEXC('RMVJOBSCDE JOB(DBTSCHED01) ENTRYNBR(*ALL)')",
    )
    .await;
    r.unwrap();
}

#[tokio::test]
#[ignore = "needs an Informix server (DBINE_TEST_ODBC_SCHED_INFORMIX)"]
async fn informix_tasks_and_groups() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_INFORMIX", "informix") else {
        return;
    };
    let mut s = open(&c).await;
    let group = round_trip(
        &mut s,
        "group",
        json!({"name": "dbine_test_sched_grp", "description": "prueba"}),
        json!({"description": "otra"}),
        ("description", json!("otra")),
    )
    .await;
    let task = round_trip(
        &mut s,
        "job",
        json!({
            "name": "dbine_test_sched_ifx", "type": "TASK", "group": "MISC", "database": "sysadmin",
            "execute": "SELECT COUNT(*) FROM systables", "start_time": "03:00:00", "frequency": 86400,
            "enabled": false,
            "list:thresholds": [{"name": "DBINE_T", "value": "1", "value_type": "NUMERIC", "description": "x"}]
        }),
        json!({"frequency": 3600, "list:thresholds": []}),
        ("frequency", json!(3600)),
    )
    .await;
    quietly(
        &mut s,
        "DELETE FROM sysadmin:ph_threshold WHERE task_name = 'dbine_test_sched_ifx'",
    )
    .await;
    quietly(
        &mut s,
        "DELETE FROM sysadmin:ph_task WHERE tk_name = 'dbine_test_sched_ifx'",
    )
    .await;
    quietly(
        &mut s,
        "DELETE FROM sysadmin:ph_group WHERE group_name = 'dbine_test_sched_grp'",
    )
    .await;
    group.unwrap();
    task.unwrap();
}

#[tokio::test]
#[ignore = "needs Hive 4 (DBINE_TEST_ODBC_SCHED_HIVE)"]
async fn hive_scheduled_queries() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_HIVE", "hive") else {
        return;
    };
    let mut s = open(&c).await;
    let r = round_trip(
        &mut s,
        "job",
        json!({
            "name": "dbine_test_sched_hive", "mode": "cron", "cron": "0 0 3 * * ? *",
            "query": "select 1", "enabled": false
        }),
        json!({"cron": "0 30 4 * * ? *"}),
        ("cron", json!("0 30 4 * * ? *")),
    )
    .await;
    quietly(&mut s, "DROP SCHEDULED QUERY dbine_test_sched_hive").await;
    r.unwrap();
}

#[tokio::test]
#[ignore = "needs Vertica 11.1.1 or later as a superuser (DBINE_TEST_ODBC_SCHED_VERTICA)"]
async fn vertica_schedules_and_triggers() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_VERTICA", "vertica") else {
        return;
    };
    let mut s = open(&c).await;
    quietly(
        &mut s,
        "CREATE PROCEDURE public.dbine_test_sched_p() LANGUAGE PLvSQL AS $$ BEGIN PERFORM 1; END; $$",
    )
    .await;
    let schedule = round_trip(
        &mut s,
        "schedule",
        json!({"name": "dbine_test_sched_vs1", "schema": "public", "kind": "cron", "cron": "0 3 * * *"}),
        json!({"cron": "30 4 * * *"}),
        ("cron", json!("30 4 * * *")),
    )
    .await;
    // The trigger needs a schedule of its own.
    let mut out = QueryOutcome::default();
    let made = s
        .execute(
            "CREATE SCHEDULE public.dbine_test_sched_vs2 USING CRON '0 3 * * *'",
            10,
            &mut out,
        )
        .await;
    let trigger = match made {
        Ok(()) => {
            round_trip(
                &mut s,
                "job",
                json!({
                    "name": "dbine_test_sched_vt", "schema": "public", "enabled": false,
                    "procedure": "\"public\".\"dbine_test_sched_p\"", "args": "",
                    "schedule": "\"public\".\"dbine_test_sched_vs2\""
                }),
                json!({"enabled": true}),
                ("enabled", json!(true)),
            )
            .await
        }
        Err(e) => Err(e.to_string()),
    };
    quietly(&mut s, "DROP TRIGGER IF EXISTS public.dbine_test_sched_vt").await;
    quietly(
        &mut s,
        "DROP SCHEDULE IF EXISTS public.dbine_test_sched_vs1",
    )
    .await;
    quietly(
        &mut s,
        "DROP SCHEDULE IF EXISTS public.dbine_test_sched_vs2",
    )
    .await;
    quietly(
        &mut s,
        "DROP PROCEDURE IF EXISTS public.dbine_test_sched_p()",
    )
    .await;
    schedule.unwrap();
    trigger.unwrap();
}

#[tokio::test]
#[ignore = "needs SAP ASE with Job Scheduler and js_admin_role (DBINE_TEST_ODBC_SCHED_ASE)"]
async fn ase_jobs_schedules_and_scheduled_jobs() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_ASE", "sybase") else {
        return;
    };
    let mut s = open(&c).await;
    let job = round_trip(
        &mut s,
        "jobdef",
        json!({"name": "dbine_test_sched_j1", "description": "prueba", "command": "select 1\ngo"}),
        json!({"command": "select 2\ngo"}),
        ("command", json!("select 2\ngo")),
    )
    .await;
    let schedule = round_trip(
        &mut s,
        "schedule",
        json!({"name": "dbine_test_sched_s1", "repeats": 2, "unit": "hours", "start_time": "08:00",
               "end_time": "18:00", "on": "days", "days": ["Monday", "Friday"]}),
        json!({"repeats": 3}),
        ("repeats", json!(3)),
    )
    .await;
    // A scheduled job needs a job and a schedule of its own.
    let mut parts = Ok(());
    for (kind, values) in [
        (
            "jobdef",
            json!({"name": "dbine_test_sched_j2", "command": "select 1\ngo"}),
        ),
        (
            "schedule",
            json!({"name": "dbine_test_sched_s2", "repeats": 1, "unit": "days", "start_time": "03:00"}),
        ),
    ] {
        let v = FormValues {
            values: serde_json::from_value(values).unwrap(),
            lists: BTreeMap::new(),
        };
        if let Err(e) = s.scheduler_save(kind, None, &v).await {
            parts = Err(e.to_string());
        }
    }
    let scheduled = match parts {
        Ok(()) => {
            round_trip(
                &mut s,
                "job",
                json!({"name": "dbine_test_sched_sj", "job": "dbine_test_sched_j2",
                       "schedule": "dbine_test_sched_s2", "enabled": false}),
                json!({"enabled": true, "timeout": 30}),
                ("timeout", json!(30)),
            )
            .await
        }
        Err(e) => Err(e),
    };
    quietly(
        &mut s,
        "exec sybmgmtdb..sp_sjobdrop @name = 'dbine_test_sched_sj', @option = 'all'",
    )
    .await;
    for name in [
        "jname=dbine_test_sched_j1",
        "jname=dbine_test_sched_j2",
        "sname=dbine_test_sched_s1",
        "sname=dbine_test_sched_s2",
    ] {
        quietly(
            &mut s,
            &format!("exec sybmgmtdb..sp_sjobdrop @name = '{name}', @option = 'all'"),
        )
        .await;
    }
    job.unwrap();
    schedule.unwrap();
    scheduled.unwrap();
}

#[tokio::test]
#[ignore = "needs SQL Anywhere with MANAGE ANY EVENT (DBINE_TEST_ODBC_SCHED_SQLA)"]
async fn sqla_events() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_SQLA", "sqlanywhere") else {
        return;
    };
    let mut s = open(&c).await;
    let scheduled = round_trip(
        &mut s,
        "job",
        json!({"name": "dbine_test_sched_ev", "mode": "schedule", "enabled": false,
               "handler": "MESSAGE 'dbine' TO CONSOLE;",
               "list:schedules": [{"name": "", "window": "start", "start_time": "03:00", "on": "week",
                                   "week": ["Monday"]}]}),
        json!({"comment": "it's a test"}),
        ("comment", json!("it's a test")),
    )
    .await;
    let typed = round_trip(
        &mut s,
        "job",
        json!({"name": "dbine_test_sched_ev2", "mode": "type", "event_type": "GrowDB", "enabled": false,
               "list:conditions": [{"condition": "DBSize", "op": ">", "value": 100000}]}),
        json!({"enabled": true}),
        ("enabled", json!(true)),
    )
    .await;
    quietly(&mut s, "DROP EVENT IF EXISTS dbine_test_sched_ev").await;
    quietly(&mut s, "DROP EVENT IF EXISTS dbine_test_sched_ev2").await;
    scheduled.unwrap();
    typed.unwrap();
}

#[tokio::test]
#[ignore = "needs InterSystems IRIS in %SYS with %Admin_Task (DBINE_TEST_ODBC_SCHED_IRIS)"]
async fn iris_tasks_are_read_only() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_IRIS", "iris") else {
        return;
    };
    let mut s = open(&c).await;
    let info = s.scheduler_info().await.unwrap();
    assert!(info
        .kinds
        .iter()
        .all(|k| !k.can_create && !k.can_edit && !k.can_delete));
    let items = s.scheduler_items("job").await.unwrap();
    let first = items.items.first().expect("IRIS always has system tasks");
    let form = s.scheduler_form("job", Some(&first.id)).await.unwrap();
    assert!(form.read_only);
    assert_eq!(form.values.get("Name"), Some(&json!(first.name)));
}

#[tokio::test]
#[ignore = "needs Dameng (DM8) as SYSDBA with the job system (DBINE_TEST_ODBC_SCHED_DAMENG)"]
async fn dameng_jobs_operators_alerts_and_dbms_job() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_DAMENG", "dameng") else {
        return;
    };
    let mut s = open(&c).await;
    let operator = round_trip(
        &mut s,
        "operator",
        json!({"name": "dbine_test_sched_op", "email": "dbine@example.com"}),
        json!({"email": "dbine2@example.com"}),
        ("email", json!("dbine2@example.com")),
    )
    .await;
    let job = round_trip(
        &mut s,
        "job",
        json!({
            "name": "dbine_test_sched_dm", "enabled": false, "description": "prueba",
            "list:steps": [{"name": "s1", "type": "0", "command": "select 1;"}],
            "list:schedules": [{"name": "daily", "type": "1", "start_time": "03:00:00",
                                "start_date": "2030-01-01T00:00:00"}]
        }),
        json!({"description": "otra"}),
        ("description", json!("otra")),
    )
    .await;
    let dbms_job = round_trip(
        &mut s,
        "dbms_job",
        json!({"what": "begin null; /* dbine_test_sched */ end;", "next_date": "2030-01-01T00:00:00",
               "interval": "SYSDATE + 1"}),
        json!({"interval": "SYSDATE + 2"}),
        ("interval", json!("SYSDATE + 2")),
    )
    .await;
    quietly(&mut s, "CALL SP_DROP_JOB('dbine_test_sched_dm')").await;
    quietly(&mut s, "CALL SP_DROP_OPERATOR('dbine_test_sched_op')").await;
    quietly(
        &mut s,
        "BEGIN FOR r IN (SELECT JOB FROM USER_JOBS WHERE WHAT LIKE '%dbine_test_sched%') LOOP DBMS_JOB.REMOVE(r.JOB); END LOOP; END",
    )
    .await;
    operator.unwrap();
    job.unwrap();
    dbms_job.unwrap();
}

#[tokio::test]
#[ignore = "needs Altibase 6.5.1 or later as SYS (DBINE_TEST_ODBC_SCHED_ALTIBASE)"]
async fn altibase_jobs() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_ALTIBASE", "altibase") else {
        return;
    };
    let mut s = open(&c).await;
    quietly(
        &mut s,
        "CREATE OR REPLACE PROCEDURE dbine_test_sched_p AS BEGIN NULL; END",
    )
    .await;
    let r = round_trip(
        &mut s,
        "job",
        json!({"name": "dbine_test_sched_aj", "procedure": "SYS.DBINE_TEST_SCHED_P",
               "start": "2030-01-01T03:00:00", "interval": 1, "unit": "DAY", "enabled": false,
               "comment": "it's a test"}),
        json!({"interval": 2}),
        ("interval", json!(2)),
    )
    .await;
    quietly(&mut s, "DROP JOB dbine_test_sched_aj").await;
    quietly(&mut s, "DROP PROCEDURE dbine_test_sched_p").await;
    r.unwrap();
}

#[tokio::test]
#[ignore = "needs Virtuoso as dba (DBINE_TEST_ODBC_SCHED_VIRTUOSO)"]
async fn virtuoso_events() {
    let Some(c) = cfg("DBINE_TEST_ODBC_SCHED_VIRTUOSO", "virtuoso") else {
        return;
    };
    let mut s = open(&c).await;
    let r = round_trip(
        &mut s,
        "job",
        json!({"name": "dbine_test_sched_vt", "start": "2030-01-01T00:00:00", "interval": 1440,
               "sql": "select 1", "enabled": false}),
        json!({"interval": 60}),
        ("interval", json!(60)),
    )
    .await;
    quietly(
        &mut s,
        "DELETE FROM DB.DBA.SYS_SCHEDULED_EVENT WHERE SE_NAME LIKE 'dbine_test_sched_%'",
    )
    .await;
    r.unwrap();
}

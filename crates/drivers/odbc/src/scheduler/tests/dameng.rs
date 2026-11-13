//! Dameng: the job system (jobs, operators, alerts), DBMS_JOB and
//! DBMS_SCHEDULER.

use super::*;
use crate::scheduler::dameng as e;

const JOB_COLS: &[&str] = &[
    "ID",
    "NAME",
    "ENABLE",
    "USERNAME",
    "ENABLE_EMAIL",
    "EMAIL_OPERID",
    "EMAIL_TYPE",
    "ENABLE_NETSEND",
    "NETSEND_OPERID",
    "NETSEND_TYPE",
    "DESCRIBE",
    "VALID",
    "SYS_FLAG",
    "CREATETIME",
    "MODIFYTIME",
];
const STEP_COLS: &[&str] = &[
    "JOBID",
    "NAME",
    "SEQNO",
    "TYPE",
    "COMMAND",
    "SUCC_ACTION",
    "FAIL_ACTION",
    "RETRY_ATTEMPTS",
    "RETRY_INTERVAL",
    "OUTPUT_FILE_PATH",
    "APPEND_FLAG",
    "DESCRIBE",
];
const SCH_COLS: &[&str] = &[
    "JOBID",
    "NAME",
    "ENABLE",
    "TYPE",
    "FREQ_INTERVAL",
    "FREQ_SUB_INTERVAL",
    "FREQ_MINUTE_INTERVAL",
    "STARTTIME",
    "ENDTIME",
    "DURING_START_DATE",
    "DURING_END_DATE",
    "DESCRIBE",
];
const SJ_COLS: &[&str] = &[
    "OWNER",
    "JOB_NAME",
    "JOB_TYPE",
    "JOB_ACTION",
    "START_DATE",
    "REPEAT_INTERVAL",
    "END_DATE",
    "ENABLED",
    "AUTO_DROP",
    "COMMENTS",
    "PROGRAM_NAME",
    "SCHEDULE_NAME",
    "NUMBER_OF_ARGUMENTS",
    "STATE",
];
const SJ: &str = "\"APP\".\"NIGHTLY_SJ\"";

/// Job 10 `nightly` of the job system (owner SYSDBA, two steps, two
/// schedules, mail to operator 1 `dba_team`), DBMS_JOB job 11 (owner
/// APP), DBMS_SCHEDULER's `APP.NIGHTLY_SJ`, alert 2 `deadlock`. The
/// session is SYSDBA when `dba`, APP otherwise.
fn server(dba: bool) -> Fake {
    server_as(dba, "SYSDBA")
}

/// [`server`] with job 10 owned by `owner`.
fn server_as(dba: bool, owner: &'static str) -> Fake {
    let agent = vec![
        Some("10"),
        Some("nightly"),
        Some("1"),
        Some(owner),
        Some("1"),
        Some("1"),
        Some("1"),
        Some("0"),
        None,
        Some("0"),
        Some("carga"),
        Some("Y"),
        Some("0"),
        Some("2026-11-01 10:00:00"),
        Some("2026-11-02 10:00:00"),
    ];
    let dbms = vec![
        Some("11"),
        Some("11"),
        Some("1"),
        Some("APP"),
        Some("0"),
        None,
        Some("0"),
        Some("0"),
        None,
        Some("0"),
        None,
        Some("Y"),
        Some("2"),
        None,
        None,
    ];
    let sj = vec![
        Some("APP"),
        Some("NIGHTLY_SJ"),
        Some("PLSQL_BLOCK"),
        Some("begin null; end;"),
        Some("2026-11-12 03:00:00.000000 +08:00"),
        Some("FREQ=DAILY"),
        None,
        Some("TRUE"),
        Some("FALSE"),
        None,
        None,
        None,
        Some("0"),
        Some("SCHEDULED"),
    ];
    Fake::default()
        .rows(
            "USER AS U",
            &["U"],
            vec![vec![Some(if dba { "SYSDBA" } else { "APP" })]],
        )
        .rows(
            "USER_ROLE_PRIVS",
            &["R"],
            if dba {
                vec![vec![Some("DBA")]]
            } else {
                Vec::new()
            },
        )
        .rows_with(
            "SYSJOB.SYSJOBS WHERE ID = ?",
            "10",
            JOB_COLS,
            vec![agent.clone()],
        )
        .rows_with(
            "SYSJOB.SYSJOBS WHERE ID = ?",
            "11",
            JOB_COLS,
            vec![dbms.clone()],
        )
        .rows("SYSJOB.SYSJOBS ORDER BY NAME", JOB_COLS, vec![agent, dbms])
        .rows_with(
            "DBA_JOBS WHERE JOB = ?",
            "11",
            &[
                "JOB",
                "WHAT",
                "NEXT_DATE",
                "INTERVAL",
                "BROKEN",
                "LAST_DATE",
                "FAILURES",
            ],
            vec![vec![
                Some("11"),
                Some("begin app.p(); end;"),
                Some("2026-11-13 00:00:00"),
                Some("86400"),
                Some("N"),
                Some("2026-11-12 00:00:00"),
                Some("0"),
            ]],
        )
        .rows(
            "FROM DBA_JOBS",
            &["JOB", "WHAT", "INTERVAL", "BROKEN"],
            vec![vec![
                Some("11"),
                Some("begin app.p(); end;"),
                Some("86400"),
                Some("N"),
            ]],
        )
        .rows(
            "SYSJOB.SYSOPERATORS ORDER BY NAME",
            &["ID", "NAME", "ENABLE", "EMAILADDR", "NETSEND_IP"],
            vec![vec![
                Some("1"),
                Some("dba_team"),
                Some("1"),
                Some("dba@example.com"),
                None,
            ]],
        )
        .rows_with(
            "SYSJOB.SYSOPERATORS WHERE ID = ?",
            "1",
            &["ID", "NAME", "ENABLE", "EMAILADDR", "NETSEND_IP"],
            vec![vec![
                Some("1"),
                Some("dba_team"),
                Some("1"),
                Some("dba@example.com"),
                None,
            ]],
        )
        .rows(
            "SYSJOB.SYSJOBSTEPS WHERE JOBID",
            STEP_COLS,
            vec![
                vec![
                    Some("10"),
                    Some("s1"),
                    Some("1"),
                    Some("0"),
                    Some("update t set a = 1;"),
                    Some("0"),
                    Some("0"),
                    Some("2"),
                    Some("30"),
                    None,
                    Some("0"),
                    None,
                ],
                vec![
                    Some("10"),
                    Some("s2"),
                    Some("2"),
                    Some("0"),
                    Some("commit;"),
                    Some("1"),
                    Some("0"),
                    Some("0"),
                    Some("0"),
                    Some("/tmp/s2.log"),
                    Some("1"),
                    Some("fin"),
                ],
            ],
        )
        .rows(
            "SYSJOB.SYSJOBSCHEDULES WHERE JOBID",
            SCH_COLS,
            vec![
                vec![
                    Some("10"),
                    Some("daily"),
                    Some("1"),
                    Some("1"),
                    Some("1"),
                    Some("0"),
                    Some("0"),
                    Some("03:00:00"),
                    None,
                    Some("2026-11-12 00:00:00"),
                    None,
                    None,
                ],
                vec![
                    Some("10"),
                    Some("weekly"),
                    Some("1"),
                    Some("2"),
                    Some("1"),
                    Some("34"),
                    Some("0"),
                    Some("05:00:00"),
                    None,
                    Some("2026-11-12 00:00:00"),
                    None,
                    None,
                ],
            ],
        )
        .rows_with(
            "DBA_SCHEDULER_JOBS WHERE OWNER",
            "NIGHTLY_SJ",
            SJ_COLS,
            vec![sj.clone()],
        )
        .rows("DBA_SCHEDULER_JOBS ORDER BY JOB_NAME", SJ_COLS, vec![sj])
        .rows_with(
            "SYSJOB.SYSALERTS WHERE ID = ?",
            "2",
            &[
                "ID",
                "NAME",
                "ENABLE",
                "TYPE",
                "ERRTYPE",
                "ERRCODE",
                "SQL_TYPE",
                "DELAY_TIME",
                "DESCRIBE",
            ],
            vec![vec![
                Some("2"),
                Some("deadlock"),
                Some("1"),
                Some("0"),
                Some("1"),
                Some("-6403"),
                Some("0"),
                Some("60"),
                Some("interbloqueos"),
            ]],
        )
        .rows(
            "ID, NAME FROM SYSJOB.SYSALERTS",
            &["ID", "NAME"],
            vec![vec![Some("2"), Some("deadlock")]],
        )
        .rows(
            "SYSJOB.SYSALERTNOTIFICATIONS",
            &["ALERTID", "OPERID", "ENABLEEMAIL", "ENABLENETSEND"],
            vec![vec![Some("2"), Some("1"), Some("1"), Some("0")]],
        )
        .rows(
            "WHERE EMAIL_OPERID = ? OR NETSEND_OPERID = ?",
            &["NAME"],
            vec![vec![Some("nightly")]],
        )
}

fn form_of(kind: &str, id: &str) -> ObjectForm {
    form(preset("dameng"), &mut server(true), &ctx(), kind, Some(id)).unwrap()
}

/// [`server`] on a DM with the run-now and stop procedures.
fn with_procs(dba: bool) -> Fake {
    server(dba).rows(
        "SUBTYPE$ = 'PROC' AND NAME IN",
        &["NAME"],
        vec![
            vec![Some("SP_DBMS_JOB_RUN_ASYNC")],
            vec![Some("SP_STOP_RUNNING_JOB")],
        ],
    )
}

#[test]
fn jobs_run_now_and_stop_and_another_users_job_is_only_read() {
    let p = preset("dameng");
    assert_eq!(
        action_script(p, &mut with_procs(true), &ctx(), "job", "10", "run").unwrap(),
        "-- Ejecuta el job ahora, en segundo plano (SP_DBMS_JOB_RUN_ASYNC).\nCALL SP_DBMS_JOB_RUN_ASYNC(10);\n"
    );
    let mut f = with_procs(true);
    action(p, &mut f, &ctx(), "dbms_job", "11", "run").unwrap();
    assert!(f.sent("CALL SP_DBMS_JOB_RUN_ASYNC(11)"));
    // Not running: nothing to stop.
    assert!(action_script(p, &mut with_procs(true), &ctx(), "job", "10", "stop").is_err());
    // DBMS_SCHEDULER: RUN_JOB away from the session, STOP_JOB.
    let s = action_script(p, &mut with_procs(true), &ctx(), "scheduler_job", SJ, "run").unwrap();
    assert!(
        s.contains("CALL DBMS_SCHEDULER.RUN_JOB(") && s.contains(", FALSE);"),
        "{s}"
    );
    let s = action_script(
        p,
        &mut with_procs(true),
        &ctx(),
        "scheduler_job",
        SJ,
        "stop",
    )
    .unwrap();
    assert!(
        s.contains("AUTO_DROP") && s.contains("CALL DBMS_SCHEDULER.STOP_JOB("),
        "{s}"
    );
    // An id of the other kind, an operator, or an action DM lacks.
    for (kind, id, act) in [
        ("job", SJ, "run"),
        ("scheduler_job", "10", "run"),
        ("operator", "1", "run"),
        ("job", "10", "open_window"),
    ] {
        let mut f = with_procs(true);
        assert!(
            action_script(p, &mut f, &ctx(), kind, id, act).is_err(),
            "{kind} {id} {act}"
        );
        assert!(!f.sent("CALL "), "{kind} {id} {act}");
    }
    // APP, not a DBA: job 10 is SYSDBA's (shown, not changed nor run),
    // DBMS_JOB job 11 its own.
    let l = items(p, &mut with_procs(false), &ctx(), "job").unwrap();
    let j = &l.items[0];
    assert_eq!(j.id, "10");
    assert_eq!((j.can_edit, j.can_delete), (Some(false), Some(false)));
    assert_eq!(j.actions, Some(Vec::new()));
    let l = items(p, &mut with_procs(false), &ctx(), "dbms_job").unwrap();
    let j = &l.items[0];
    assert_eq!((j.can_edit, j.can_delete), (None, None));
    assert_eq!(j.actions, Some(vec!["run".to_string()]));
    assert!(action_script(p, &mut with_procs(false), &ctx(), "job", "10", "run").is_err());
}

#[test]
fn a_running_job_stops_with_sp_stop_running_job() {
    let p = preset("dameng");
    // Job 10 is running now.
    let running = || {
        with_procs(true).rows(
            "V$JOBS_RUNNING",
            &["JOBID", "THIS_DATE"],
            vec![vec![Some("10"), Some("2026-11-12 03:00:00")]],
        )
    };
    let l = items(p, &mut running(), &ctx(), "job").unwrap();
    let j = l.items.iter().find(|i| i.id == "10").unwrap();
    assert!(
        j.actions
            .as_ref()
            .is_some_and(|a| a.iter().any(|k| k == "stop")),
        "{:?}",
        j.actions
    );
    assert_eq!(
        action_script(p, &mut running(), &ctx(), "job", "10", "stop").unwrap(),
        "-- Corta la sesión del job en ejecución (SP_STOP_RUNNING_JOB); su programación sigue igual.\nCALL SP_STOP_RUNNING_JOB(10);\n"
    );
    let mut f = running();
    action(p, &mut f, &ctx(), "job", "10", "stop").unwrap();
    assert!(f.sent("CALL SP_STOP_RUNNING_JOB(10)"));
}

#[test]
fn an_owner_who_isnt_a_dba_clears_the_history_of_their_own_jobs_only() {
    let p = preset("dameng");
    // APP's own DBMS_JOB job 11: SP_JOB_CLEAR_HISTORIES, which DM lets a
    // user run on their own jobs.
    let mut f = server(false);
    clear_history(p, &mut f, "11").unwrap();
    assert!(f.sent("CALL SP_JOB_CLEAR_HISTORIES('11')"), "{:?}", f.log);
    // SYSDBA's job 10: refused before the server.
    let mut f = server(false);
    let r = clear_history(p, &mut f, "10");
    assert!(
        matches!(&r, Err(Error::Unsupported(m)) if m.contains("Solo un DBA o el dueño")),
        "{r:?}"
    );
    assert!(!f.sent("SP_JOB_CLEAR_HISTORIES"));
}

fn sc(kind: &str, id: Option<&str>, v: &FormValues) -> Result<String> {
    script(preset("dameng"), &mut server(true), kind, id, v)
}

#[test]
fn a_new_job_is_created_and_configured_in_one_transaction() {
    let v = fv(
        json!({"name": "load", "enabled": true, "description": "carga diaria", "email": true,
               "email_operator": "dba_team", "email_type": "1", "netsend": false}),
        json!({
            "steps": [{"name": "s1", "type": "0", "command": "insert into t select 1;", "on_success": "0",
                        "on_failure": "0", "retries": 1, "retry_interval": 10, "append": false}],
            "schedules": [{"name": "nightly", "enabled": true, "type": "1", "every": 1, "minutes": 0,
                            "start_time": "03:00:00", "start_date": "2026-11-20T00:00:00"}]
        }),
    );
    assert_eq!(
        sc("job", None, &v).unwrap(),
        "-- Job «load» de DM
CALL SP_CREATE_JOB('load', 1, 1, 'dba_team', 1, 0, '', 1, 'carga diaria');
CALL SP_JOB_CONFIG_START('load');
CALL SP_ADD_JOB_STEP_EX('load', 's1', 0, 'insert into t select 1;', 0, 0, 1, 10, NULL, 0, '');
CALL SP_ADD_JOB_SCHEDULE('load', 'nightly', 1, 1, 1, 0, 0, '03:00:00', NULL, '2026-11-20 00:00:00', NULL, '');
CALL SP_JOB_CONFIG_COMMIT('load');
"
    );
    let mut f = server(true).rows_with(
        "SELECT ID FROM SYSJOB.SYSJOBS WHERE NAME = ?",
        "load",
        &["ID"],
        vec![vec![Some("12")]],
    );
    let id = save(preset("dameng"), &mut f, "job", None, &v).unwrap();
    assert_eq!(id.as_deref(), Some("12"));
    assert_eq!(f.tx, ["begin", "commit"]);
}

#[test]
fn a_job_reads_back_whole_and_changes_by_parts() {
    let f = form_of("job", "10");
    well_formed(&f);
    assert_eq!(val(&f, "email_operator"), "dba_team");
    assert_eq!(val(&f, "email_type"), "1");
    assert_eq!(val(&f, "owner"), "SYSDBA");
    let steps = &f.sections[2].lists[0].rows;
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[1]["output"], "/tmp/s2.log");
    let scheds = &f.sections[3].lists[0].rows;
    assert_eq!(scheds[1]["week_days"], json!(["2", "32"]));
    assert_eq!(scheds[0]["start_date"], "2026-11-12T00:00:00");
    assert_eq!(
        sc("job", Some("10"), &edit(&f, json!({}))).unwrap(),
        "-- «nightly»: no hay cambios que guardar.\n"
    );
    let mut s1 = steps[0].clone();
    s1.insert("retries".into(), json!(3));
    let mut daily = scheds[0].clone();
    daily.insert("minutes".into(), json!(30));
    daily.insert("end_time".into(), json!("06:00:00"));
    let v = edit(
        &f,
        json!({
            "description": "otra",
            "list:steps": [s1, steps[1].clone()],
            "list:schedules": [daily, {"name": "monthly", "type": "3", "month_day": 1,
                                       "start_date": "2026-11-12T00:00:00"}]
        }),
    );
    assert_eq!(
        sc("job", Some("10"), &v).unwrap(),
        "-- Job «nightly» de DM
CALL SP_JOB_CONFIG_START('nightly');
CALL SP_ALTER_JOB('nightly', 1, 1, 'dba_team', 1, 0, '', 0, 'otra');
CALL SP_DROP_JOB_STEP('nightly', 's1');
CALL SP_DROP_JOB_STEP('nightly', 's2');
CALL SP_ADD_JOB_STEP_EX('nightly', 's1', 0, 'update t set a = 1;', 0, 0, 3, 30, NULL, 0, '');
CALL SP_ADD_JOB_STEP_EX('nightly', 's2', 0, 'commit;', 1, 0, 0, 0, '/tmp/s2.log', 1, 'fin');
CALL SP_DROP_JOB_SCHEDULE('nightly', 'weekly');
CALL SP_ALTER_JOB_SCHEDULE('nightly', 'daily', 1, 1, 1, 0, 30, '03:00:00', '06:00:00', '2026-11-12 00:00:00', NULL, '');
CALL SP_ADD_JOB_SCHEDULE('nightly', 'monthly', 1, 3, 1, 1, 0, '00:00:00', NULL, '2026-11-12 00:00:00', NULL, '');
CALL SP_JOB_CONFIG_COMMIT('nightly');
"
    );
    // A new name is SP_RENAME_JOB: the same job (id 10), nothing dropped
    // nor created again.
    assert_eq!(
        sc("job", Some("10"), &edit(&f, json!({"name": "nightly2"}))).unwrap(),
        "-- Job «nightly» de DM\nCALL SP_RENAME_JOB('nightly', 'nightly2');\n"
    );
    // Renamed and changed: the rest goes under the new name.
    let v = edit(
        &f,
        json!({
            "name": "nightly2",
            "description": "otra",
            "list:schedules": [scheds[0].clone()]
        }),
    );
    assert_eq!(
        sc("job", Some("10"), &v).unwrap(),
        "-- Job «nightly» de DM
CALL SP_RENAME_JOB('nightly', 'nightly2');
CALL SP_JOB_CONFIG_START('nightly2');
CALL SP_ALTER_JOB('nightly2', 1, 1, 'dba_team', 1, 0, '', 0, 'otra');
CALL SP_DROP_JOB_SCHEDULE('nightly2', 'weekly');
CALL SP_JOB_CONFIG_COMMIT('nightly2');
"
    );
    let mut srv = server(true);
    let id = save(preset("dameng"), &mut srv, "job", Some("10"), &v).unwrap();
    assert_eq!(id.as_deref(), Some("10"));
    assert!(!srv.sent("SP_DROP_JOB(") && !srv.sent("SP_CREATE_JOB("));
}

#[test]
fn a_dba_renames_another_users_job_in_place() {
    // SYSDBA manages APP's job 10: SP_RENAME_JOB keeps APP its owner.
    let mut srv = server_as(true, "APP");
    let f = form(preset("dameng"), &mut srv, &ctx(), "job", Some("10")).unwrap();
    assert!(!f.read_only);
    assert!(
        !f.notes.iter().any(|n| n.contains("dueño")),
        "{:?}",
        f.notes
    );
    let s = script(
        preset("dameng"),
        &mut server_as(true, "APP"),
        "job",
        Some("10"),
        &edit(&f, json!({"name": "nightly2"})),
    )
    .unwrap();
    assert!(
        s.contains("CALL SP_RENAME_JOB('nightly', 'nightly2');"),
        "{s}"
    );
    // APP, not a DBA, doesn't rename SYSDBA's job.
    let f = form(
        preset("dameng"),
        &mut server(false),
        &ctx(),
        "job",
        Some("10"),
    )
    .unwrap();
    let r = script(
        preset("dameng"),
        &mut server(false),
        "job",
        Some("10"),
        &edit(&f, json!({"name": "nightly2"})),
    );
    assert!(
        matches!(&r, Err(Error::Unsupported(m)) if m.contains("Solo un DBA o el dueño")),
        "{r:?}"
    );
}

#[test]
fn job_values_are_checked() {
    let ops = vec![("1".to_string(), "dba_team".to_string())];
    let step = json!({"name": "s1", "command": "select 1;"});
    for (vals, lists, want) in [
        (
            json!({"name": "j"}),
            json!({"steps": []}),
            "al menos un paso",
        ),
        (
            json!({"name": "j", "email": true}),
            json!({"steps": [step]}),
            "Falta «Operador (correo)».",
        ),
        (
            json!({"name": "j", "email": true, "email_operator": "nadie"}),
            json!({"steps": [step]}),
            "No existe el operador",
        ),
        (
            json!({"name": "j"}),
            json!({"steps": [step, step]}),
            "está repetido",
        ),
        (
            json!({"name": "j"}),
            json!({"steps": [step], "schedules": [{"name": "w", "type": "2", "start_date": "2026-11-12T00:00:00"}]}),
            "al menos un día",
        ),
        (
            json!({"name": "j"}),
            json!({"steps": [step], "schedules": [{"name": "m", "type": "3", "month_day": 40, "start_date": "2026-11-12T00:00:00"}]}),
            "«Día del mes»",
        ),
        (
            json!({"name": "j"}),
            json!({"steps": [step], "schedules": [{"name": "o", "type": "0"}]}),
            "«Desde»",
        ),
        (
            json!({"name": "j"}),
            json!({"steps": [{"name": "s1", "command": "x", "type": "9"}]}),
            "«Tipo»",
        ),
    ] {
        let m = refused(e::agent_from(&fv(vals.clone(), lists), &ops, true));
        assert!(m.contains(want), "{vals}: {m}");
    }
}

#[test]
fn dbms_job_jobs_open_from_the_job_kind_and_change_with_the_package() {
    let f = form_of("job", "11");
    well_formed(&f);
    assert_eq!(val(&f, "what"), "begin app.p(); end;");
    assert_eq!(val(&f, "interval"), "SYSDATE + 86400/86400");
    assert_eq!(val(&f, "next_date"), "2026-11-13T00:00:00");
    let v = edit(&f, json!({"interval": "SYSDATE + 1", "broken": true}));
    assert_eq!(
        sc("dbms_job", Some("11"), &v).unwrap(),
        "-- Job 11 de DBMS_JOB de DM
CALL DBMS_JOB.INTERVAL(11, 'SYSDATE + 1');
CALL DBMS_JOB.BROKEN(11, TRUE);
"
    );
    let v = fv(
        json!({"what": "begin app.p(); end;", "next_date": "2026-11-20T00:00:00", "interval": "SYSDATE + 1"}),
        json!({}),
    );
    assert_eq!(
        sc("dbms_job", None, &v).unwrap(),
        "-- Job de DBMS_JOB de DM
DECLARE
  JOB_NO INT;
BEGIN
  DBMS_JOB.SUBMIT(JOB_NO, 'begin app.p(); end;', CAST('2026-11-20 00:00:00' AS DATETIME), 'SYSDATE + 1');
END;
"
    );
    assert_eq!(
        delete_script(preset("dameng"), &mut server(true), "job", "11").unwrap(),
        "-- Borra el job 11 de DBMS_JOB de DM\nCALL DBMS_JOB.REMOVE(11);\n"
    );
}

#[test]
fn dbms_scheduler_jobs_change_attribute_by_attribute() {
    let f = form_of("job", SJ);
    well_formed(&f);
    assert_eq!(val(&f, "start"), "2026-11-12T03:00:00");
    assert_eq!(val(&f, "owner"), "APP");
    assert_eq!(
        sc("scheduler_job", Some(SJ), &edit(&f, json!({}))).unwrap(),
        "-- «NIGHTLY_SJ»: no hay cambios que guardar.\n"
    );
    let v = edit(
        &f,
        json!({"repeat": "FREQ=HOURLY", "comments": "x", "enabled": false}),
    );
    assert_eq!(
        sc("scheduler_job", Some(SJ), &v).unwrap(),
        "-- Job «NIGHTLY_SJ» de DBMS_SCHEDULER de DM
BEGIN
  DBMS_SCHEDULER.SET_ATTRIBUTE('APP.NIGHTLY_SJ', 'REPEAT_INTERVAL', 'FREQ=HOURLY');
  DBMS_SCHEDULER.SET_ATTRIBUTE('APP.NIGHTLY_SJ', 'COMMENTS', 'x');
  DBMS_SCHEDULER.DISABLE('APP.NIGHTLY_SJ');
END;
"
    );
    let s = sc(
        "scheduler_job",
        Some(SJ),
        &edit(&f, json!({"name": "other"})),
    )
    .unwrap();
    assert!(
        s.contains("CALL DBMS_SCHEDULER.DROP_JOB('APP.NIGHTLY_SJ');"),
        "{s}"
    );
    // SYSDBA saves it, APP keeps it: the new job goes in APP's schema.
    assert!(s.contains("JOB_NAME => 'APP.OTHER'"), "{s}");
    let mut f = server(true);
    let id = save(
        preset("dameng"),
        &mut f,
        "scheduler_job",
        Some(SJ),
        &edit(&form_of("job", SJ), json!({"name": "Other"})),
    )
    .unwrap();
    assert_eq!(id.as_deref(), Some("\"APP\".\"OTHER\""));
    let s = sc(
        "scheduler_job",
        Some(SJ),
        &edit(&form_of("job", SJ), json!({"name": "Mixed Case"})),
    )
    .unwrap();
    assert!(s.contains("JOB_NAME => '\"APP\".\"Mixed Case\"'"), "{s}");
    let v = fv(
        json!({"name": "nightly_sj", "job_type": "PLSQL_BLOCK", "action": "begin null; end;",
               "start": "2026-11-20T03:00:00", "repeat": "FREQ=DAILY;BYHOUR=3", "enabled": true}),
        json!({}),
    );
    assert_eq!(
        sc("scheduler_job", None, &v).unwrap(),
        "-- Job «nightly_sj» de DBMS_SCHEDULER de DM
BEGIN
  DBMS_SCHEDULER.CREATE_JOB(
    JOB_NAME => 'NIGHTLY_SJ',
    JOB_TYPE => 'PLSQL_BLOCK',
    JOB_ACTION => 'begin null; end;',
    START_DATE => TIMESTAMP '2026-11-20 03:00:00',
    REPEAT_INTERVAL => 'FREQ=DAILY;BYHOUR=3',
    ENABLED => TRUE,
    AUTO_DROP => FALSE
  );
END;
"
    );
    let mut f = server(true);
    clear_history(preset("dameng"), &mut f, SJ).unwrap();
    assert!(f.sent("CALL DBMS_SCHEDULER.PURGE_LOG(0, 'JOB_LOG', 'APP.NIGHTLY_SJ')"));
}

#[test]
fn dbms_scheduler_jobs_keep_their_program_type_and_named_schedule() {
    // PROG_SJ runs program APP.P1 on schedule APP.S1 (no type, no action,
    // no times of its own); EXE_SJ is an EXECUTABLE job.
    let row = |name: &'static str, ty: Option<&'static str>, prog: Option<&'static str>| {
        vec![
            Some("APP"),
            Some(name),
            ty,
            ty.map(|_| "/bin/true"),
            None,
            None,
            None,
            Some("TRUE"),
            Some("FALSE"),
            None,
            prog,
            prog.map(|_| "APP.S1"),
            Some("0"),
            Some("SCHEDULED"),
        ]
    };
    let srv = || {
        server(true)
            .rows_with(
                "DBA_SCHEDULER_JOBS WHERE OWNER",
                "PROG_SJ",
                SJ_COLS,
                vec![row("PROG_SJ", None, Some("APP.P1"))],
            )
            .rows_with(
                "DBA_SCHEDULER_JOBS WHERE OWNER",
                "EXE_SJ",
                SJ_COLS,
                vec![row("EXE_SJ", Some("EXECUTABLE"), None)],
            )
    };
    let prog = "\"APP\".\"PROG_SJ\"";
    let f = form(preset("dameng"), &mut srv(), &ctx(), "job", Some(prog)).unwrap();
    well_formed(&f);
    assert!(f
        .notes
        .iter()
        .any(|n| n.contains("no puede cambiar de nombre")));
    assert!(f.notes.iter().any(|n| n.contains("corre un programa")));
    assert!(f.notes.iter().any(|n| n.contains("calendario con nombre")));
    // The read-only parts come back empty: enabling or a comment still
    // saves, without touching what it runs or when.
    let v = edit(
        &f,
        json!({"enabled": false, "comments": "x", "action": null, "start": null}),
    );
    assert_eq!(
        script(
            preset("dameng"),
            &mut srv(),
            "scheduler_job",
            Some(prog),
            &v
        )
        .unwrap(),
        "-- Job «PROG_SJ» de DBMS_SCHEDULER de DM
BEGIN
  DBMS_SCHEDULER.SET_ATTRIBUTE('APP.PROG_SJ', 'COMMENTS', 'x');
  DBMS_SCHEDULER.DISABLE('APP.PROG_SJ');
END;
"
    );
    // A new name would recreate it without its program and schedule.
    let m = refused(script(
        preset("dameng"),
        &mut srv(),
        "scheduler_job",
        Some(prog),
        &edit(&f, json!({"name": "other"})),
    ));
    assert!(m.contains("no puede cambiar de nombre"), "{m}");

    let exe = "\"APP\".\"EXE_SJ\"";
    let f = form(preset("dameng"), &mut srv(), &ctx(), "job", Some(exe)).unwrap();
    assert!(f
        .notes
        .iter()
        .any(|n| n.contains("de un tipo que DBine no escribe")));
    let s = script(
        preset("dameng"),
        &mut srv(),
        "scheduler_job",
        Some(exe),
        &edit(&f, json!({"repeat": "FREQ=HOURLY"})),
    )
    .unwrap();
    assert!(s.contains("'REPEAT_INTERVAL', 'FREQ=HOURLY'"), "{s}");
    assert!(!s.contains("JOB_TYPE") && !s.contains("JOB_ACTION"), "{s}");
}

#[test]
fn operators_and_alerts() {
    let v = fv(
        json!({"name": "ops2", "email": "ops@example.com"}),
        json!({}),
    );
    assert_eq!(
        sc("operator", None, &v).unwrap(),
        "-- Operador «ops2» de DM\nCALL SP_CREATE_OPERATOR('ops2', 1, 'ops@example.com', NULL);\n"
    );
    let f = form_of("operator", "1");
    well_formed(&f);
    assert_eq!(val(&f, "used_by"), "job «nightly», alerta «deadlock»");
    assert_eq!(
        sc("operator", Some("1"), &edit(&f, json!({"email": "new@example.com"}))).unwrap(),
        "-- Operador «dba_team» de DM\nCALL SP_ALTER_OPERATOR('dba_team', 1, 'new@example.com', NULL);\n"
    );
    assert!(
        refused(sc("operator", Some("1"), &edit(&f, json!({"name": "x"}))))
            .contains("no cambia el nombre")
    );
    assert!(refused(delete_script(
        preset("dameng"),
        &mut server(true),
        "operator",
        "1"
    ))
    .contains("job «nightly»"));

    let f = form_of("alert", "2");
    well_formed(&f);
    assert_eq!(val(&f, "err_code"), -6403);
    let rows = &f.sections[2].lists[0].rows;
    assert_eq!(rows[0]["operator"], "dba_team");
    let v = edit(
        &f,
        json!({"delay": 120, "list:notify": [{"operator": "dba_team", "email": false, "netsend": true}]}),
    );
    assert_eq!(
        sc("alert", Some("2"), &v).unwrap(),
        "-- Alerta «deadlock» de DM
CALL SP_ALTER_ALERT('deadlock', 1, 0, 1, -6403, 0, 120, 'interbloqueos');
CALL SP_ALERT_DROP_OPERATOR('deadlock', 'dba_team');
CALL SP_ALERT_ADD_OPERATOR('deadlock', 'dba_team', 0, 1);
"
    );
    let v = fv(
        json!({"name": "a1", "err_code": -1}),
        json!({"notify": [{"operator": "dba_team"}]}),
    );
    assert_eq!(
        sc("alert", None, &v).unwrap(),
        "-- Alerta «a1» de DM
CALL SP_CREATE_ALERT('a1', 1, 0, 0, -1, 0, 0, '');
CALL SP_ALERT_ADD_OPERATOR('a1', 'dba_team', 1, 0);
"
    );
    assert_eq!(
        delete_script(preset("dameng"), &mut server(true), "alert", "2").unwrap(),
        "-- Borra la alerta «deadlock» de DM\nCALL SP_DROP_ALERT('deadlock');\n"
    );
}

#[test]
fn who_may_change_what() {
    // APP isn't a DBA: job 10 (SYSDBA's) is only read, operators too.
    let f = form(
        preset("dameng"),
        &mut server(false),
        &ctx(),
        "job",
        Some("10"),
    )
    .unwrap();
    assert!(f.read_only && f.notes[0].contains("dueño"));
    assert!(matches!(
        script(
            preset("dameng"),
            &mut server(false),
            "job",
            Some("10"),
            &edit(&f, json!({}))
        ),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        script(
            preset("dameng"),
            &mut server(false),
            "operator",
            None,
            &fv(json!({"name": "o"}), json!({}))
        ),
        Err(Error::Unsupported(_))
    ));
    // Job 11 is APP's own.
    let f = form(
        preset("dameng"),
        &mut server(false),
        &ctx(),
        "dbms_job",
        Some("11"),
    )
    .unwrap();
    assert!(!f.read_only);
    let i = info(preset("dameng"), &mut server(false), &ctx()).unwrap();
    let op = i.kinds.iter().find(|k| k.key == "operator").unwrap();
    assert!(!op.can_create);
}

#[test]
fn info_lists_clear_history_and_steps() {
    let i = info(preset("dameng"), &mut server(true), &ctx()).unwrap();
    let keys: Vec<&str> = i.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(
        keys,
        ["job", "dbms_job", "operator", "alert", "scheduler_job"]
    );
    for (kind, id) in [("job", "10"), ("dbms_job", "11"), ("scheduler_job", SJ)] {
        let l = items(preset("dameng"), &mut server(true), &ctx(), kind).unwrap();
        assert_eq!(l.items.len(), 1, "{kind}: {l:?}");
        assert_eq!(l.items[0].id, id);
    }
    let l = items(preset("dameng"), &mut server(true), &ctx(), "operator").unwrap();
    assert_eq!(l.items[0].name, "dba_team");
    let mut f = server(true);
    clear_history(preset("dameng"), &mut f, "10").unwrap();
    // DM's own procedure, not DELETEs on SYSJOB's tables.
    assert!(f.sent("CALL SP_JOB_CLEAR_HISTORIES('nightly')"));
    assert!(!f.sent("DELETE FROM SYSJOB"));
    assert_eq!(f.tx, ["begin", "commit"]);
    // The job kinds: run now, stop and clear history, never a step.
    for k in i.kinds.iter().filter(|k| k.key.ends_with("job")) {
        let keys: Vec<&str> = k.actions.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["run", "stop"], "{}", k.key);
        assert!(k.clear_history && !k.run_from_step, "{}", k.key);
    }
    for k in i.kinds.iter().filter(|k| !k.key.ends_with("job")) {
        assert!(k.actions.is_empty() && !k.clear_history, "{}", k.key);
    }
    assert!(matches!(
        run_from(preset("dameng"), "10", "s2"),
        Err(Error::Unsupported(m)) if m.contains("primer paso")
    ));
    for k in ["job", "dbms_job", "scheduler_job", "operator", "alert"] {
        well_formed(&form(preset("dameng"), &mut server(true), &ctx(), k, None).unwrap());
    }
    assert_eq!(
        delete_script(preset("dameng"), &mut server(true), "job", "10").unwrap(),
        "-- Borra el job «nightly» de DM, con sus pasos y programaciones\nCALL SP_DROP_JOB('nightly');\n"
    );
}

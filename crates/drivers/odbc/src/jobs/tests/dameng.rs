//! Dameng's job system.

use super::*;
use crate::jobs::dameng::fixtures::*;
use crate::jobs::dameng::{describe, Tables};

const JOB_COLS: &[&str] = &[
    "ID",
    "NAME",
    "ENABLE",
    "USERNAME",
    "CREATETIME",
    "MODIFYTIME",
    "ENABLE_EMAIL",
    "EMAIL_OPERID",
    "EMAIL_TYPE",
    "ENABLE_NETSEND",
    "NETSEND_OPERID",
    "NETSEND_TYPE",
    "VALID",
    "DESCRIBE",
    "SYS_FLAG",
];

fn test_job() -> Vec<Option<&'static str>> {
    vec![
        Some("1680153662"),
        Some("TEST"),
        Some("1"),
        Some("SYSDBA"),
        Some("2026-01-01 10:00:00"),
        Some("2026-01-02 10:00:00"),
        Some("1"),
        Some("1"),
        Some("2"),
        Some("0"),
        Some("0"),
        Some("0"),
        Some("Y"),
        Some("Every test"),
        Some("0"),
    ]
}

fn dbms_job() -> Vec<Option<&'static str>> {
    vec![
        Some("1680153663"),
        Some("DBMSJ"),
        Some("1"),
        Some("APP"),
        None,
        None,
        Some("0"),
        None,
        Some("0"),
        Some("0"),
        None,
        Some("0"),
        Some("Y"),
        None,
        Some("2"),
    ]
}

fn jobs() -> Vec<Vec<Option<&'static str>>> {
    vec![
        test_job(),
        dbms_job(),
        vec![
            Some("1680153664"),
            Some("HALF"),
            Some("0"),
            Some("APP"),
            None,
            None,
            Some("0"),
            None,
            Some("0"),
            Some("0"),
            None,
            Some("0"),
            Some("N"),
            None,
            Some("0"),
        ],
    ]
}

const SCHEDULE_COLS: &[&str] = &[
    "ID",
    "NAME",
    "JOBID",
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

#[allow(clippy::too_many_arguments)]
fn schedule(
    name: &'static str,
    job: &'static str,
    enable: &'static str,
    t: &'static str,
    freq: &'static str,
    sub: &'static str,
    minutes: &'static str,
    times: (Option<&'static str>, Option<&'static str>),
    dates: (Option<&'static str>, Option<&'static str>),
    describe: Option<&'static str>,
) -> Vec<Option<&'static str>> {
    vec![
        Some("1"),
        Some(name),
        Some(job),
        Some(enable),
        Some(t),
        Some(freq),
        Some(sub),
        Some(minutes),
        times.0,
        times.1,
        dates.0,
        dates.1,
        describe,
    ]
}

fn schedules() -> Vec<Vec<Option<&'static str>>> {
    vec![
        schedule(
            "S1",
            "1680153662",
            "1",
            "2",
            "1",
            "68",
            "0",
            (Some("03:00:00"), None),
            (Some("2026-01-01"), None),
            Some("weekly"),
        ),
        schedule(
            "S2",
            "1680153662",
            "1",
            "1",
            "1",
            "0",
            "15",
            (Some("08:00:00"), Some("18:00:00")),
            (Some("2026-01-01"), Some("2026-12-31")),
            None,
        ),
        schedule(
            "S3",
            "1680153662",
            "0",
            "8",
            "2",
            "6",
            "0",
            (Some("23:00:00"), None),
            (Some("2026-01-01 00:00:00"), None),
            None,
        ),
        schedule(
            "DBMS",
            "1680153663",
            "1",
            "9",
            "-1",
            "-1",
            "-1",
            (None, None),
            (Some("2026-01-01"), None),
            None,
        ),
    ]
}

const STEP_COLS: &[&str] = &[
    "ID",
    "NAME",
    "JOBID",
    "SEQNO",
    "TYPE",
    "DBNAME",
    "COMMAND",
    "SUCC_ACTION",
    "FAIL_ACTION",
    "RETRY_ATTEMPTS",
    "RETRY_INTERVAL",
    "OUTPUT_FILE_PATH",
    "APPEND_FLAG",
    "SUCC_NEXT_STEP",
    "FAIL_NEXT_STEP",
    "DESCRIBE",
];

fn steps() -> Vec<Vec<Option<&'static str>>> {
    vec![
        vec![
            Some("1"),
            Some("STEP1"),
            Some("1680153662"),
            Some("1"),
            Some("0"),
            Some("DAMENG"),
            Some("insert into myinfo values(1000, 'Hello World');"),
            Some("3"),
            Some("1"),
            Some("2"),
            Some("1"),
            Some("/dm/log/step1.log"),
            Some("0"),
            Some("STEP2"),
            None,
            Some("first"),
        ],
        vec![
            Some("2"),
            Some("STEP2"),
            Some("1680153662"),
            Some("2"),
            Some("3"),
            Some("DAMENG"),
            None,
            Some("1"),
            Some("1"),
            Some("0"),
            Some("0"),
            None,
            Some("0"),
            None,
            None,
            None,
        ],
    ]
}

const HISTORY_COLS: &[&str] = &[
    "EXEC_ID",
    "NAME",
    "START_TIME",
    "END_TIME",
    "ERRCODE",
    "ERRINFO",
    "JOBID",
];

fn dbms_jobs() -> Vec<Vec<Option<&'static str>>> {
    vec![vec![
        Some("1680153663"),
        Some("APP"),
        Some("2026-11-12 08:00:00"),
        Some("2026-11-12 10:00:00"),
        Some("N"),
        Some("3600"),
        Some("0"),
        Some("TEST_PROC;"),
    ]]
}

const DBMS_JOB_COLS: &[&str] = &[
    "JOB",
    "LOG_USER",
    "LAST_DATE",
    "NEXT_DATE",
    "BROKEN",
    "INTERVAL",
    "FAILURES",
    "WHAT",
];

/// A server where the user reads `t`'s tables (and not the others when
/// `t` is the views), connected as `user` with `roles`.
fn fake(t: Tables, user: &'static str, roles: &[&'static str]) -> Fake {
    let mut f = Fake::default();
    if t.own {
        f = f
            .fail(key(format!("FROM {} ", ALL.jobs)), None)
            .fail(key(format!("FROM {} ", ALL.histories)), None);
    }
    f.rows(
        NOW,
        &["LOCAL_NOW", "UTC_NOW"],
        vec![vec![Some(MINUS_3.0), Some(MINUS_3.1)]],
    )
    .rows(USER, &["U"], vec![vec![Some(user)]])
    .rows(
        ROLES,
        &["R"],
        roles.iter().map(|r| vec![Some(*r)]).collect(),
    )
    .rows(
        RUNNING,
        &["JOBID", "THIS_DATE"],
        vec![vec![Some("1680153662"), Some("2026-11-12 08:59:00")]],
    )
    .rows(
        OPERATORS,
        &["ID", "NAME"],
        vec![vec![Some("1"), Some("TOM")]],
    )
    .rows(
        key(format!("FROM {} WHERE ID = ?", t.jobs)),
        JOB_COLS,
        vec![test_job()],
    )
    .rows(
        key(format!("FROM {} ORDER BY NAME", t.jobs)),
        JOB_COLS,
        jobs(),
    )
    .rows(
        key(format!("FROM {}", t.schedules)),
        SCHEDULE_COLS,
        schedules(),
    )
    .rows(key(format!("FROM {}", t.steps)), STEP_COLS, steps())
    .rows(
        key(format!("FROM {} WHERE EXEC_ID IN", t.histories)),
        HISTORY_COLS,
        vec![
            vec![
                Some("900"),
                Some("TEST"),
                Some("2026-11-12 03:00:00"),
                Some("2026-11-12 03:00:02"),
                Some("0"),
                None,
                Some("1680153662"),
            ],
            vec![
                Some("901"),
                Some("DBMSJ"),
                Some("2026-11-12 08:00:00"),
                Some("2026-11-12 08:00:01"),
                Some("-2106"),
                Some("无效的表或视图名[X]"),
                Some("1680153663"),
            ],
        ],
    )
    .rows(
        key(format!("FROM {} WHERE NAME = ?", t.histories)),
        HISTORY_COLS,
        vec![
            vec![
                Some("900"),
                Some("TEST"),
                Some("2026-11-12 03:00:00"),
                Some("2026-11-12 03:00:02"),
                Some("0"),
                None,
                Some("1680153662"),
            ],
            vec![
                Some("880"),
                Some("TEST"),
                Some("2026-11-11 03:00:00"),
                Some("2026-11-11 03:00:05"),
                Some("-6102"),
                None,
                Some("1680153662"),
            ],
        ],
    )
    .rows(
        key(format!("FROM {} WHERE NAME = ?", t.step_histories)),
        &[
            "EXEC_ID",
            "NAME",
            "STEPNAME",
            "START_TIME",
            "END_TIME",
            "ERRTYPE",
            "ERRCODE",
            "ERRINFO",
            "RETRY_ATTEMPTS",
        ],
        vec![
            vec![
                Some("880"),
                Some("TEST"),
                Some("STEP1"),
                Some("2026-11-11 03:00:01"),
                Some("2026-11-11 03:00:04"),
                Some("0"),
                Some("-6102"),
                Some("违反唯一性约束"),
                Some("2"),
            ],
            vec![
                Some("900"),
                Some("TEST"),
                Some("STEP1"),
                Some("2026-11-12 03:00:00"),
                Some("2026-11-12 03:00:01"),
                Some("0"),
                Some("0"),
                None,
                Some("0"),
            ],
        ],
    )
    .rows(
        key(format!("FROM {}", t.dbms_jobs)),
        DBMS_JOB_COLS,
        dbms_jobs(),
    )
}

fn dm() -> &'static Preset {
    preset("dameng")
}

#[test]
fn dameng_describes_schedules_like_its_manager() {
    let s = Set::new(SCHEDULE_COLS, schedules());
    assert_eq!(
        describe(&s, 0).as_deref(),
        Some("Todas las semanas: martes y sábado, a las 03:00:00")
    );
    assert_eq!(
        describe(&s, 1).as_deref(),
        Some("Todos los días, cada 15 minutos entre las 08:00:00 y las 18:00:00, hasta el 2026-12-31")
    );
    assert_eq!(
        describe(&s, 2).as_deref(),
        Some("El último viernes, cada 2 meses, a las 23:00:00 (deshabilitada)")
    );
    assert_eq!(describe(&s, 3), None, "DBMS_JOB's");
    let one = |t, freq, sub, minutes, times, dates| {
        let s = Set::new(
            SCHEDULE_COLS,
            vec![schedule(
                "X", "1", "1", t, freq, sub, minutes, times, dates, None,
            )],
        );
        describe(&s, 0).unwrap()
    };
    assert_eq!(
        one(
            "0",
            "0",
            "0",
            "0",
            (None, None),
            (Some("2026-12-24 20:00:00"), None)
        ),
        "Una vez, el 2026-12-24 20:00:00"
    );
    assert_eq!(
        one(
            "1",
            "3",
            "0",
            "1",
            (Some("00:00:00"), None),
            (Some("2026-01-01"), None)
        ),
        "Cada 3 días, cada minuto desde las 00:00:00"
    );
    assert_eq!(
        one(
            "2",
            "2",
            "1",
            "0",
            (Some("06:00:00"), None),
            (Some("2026-01-01"), None)
        ),
        "Cada 2 semanas: domingo, a las 06:00:00"
    );
    assert_eq!(
        one(
            "3",
            "1",
            "15",
            "0",
            (Some("01:00:00"), None),
            (Some("2026-01-01"), None)
        ),
        "El día 15 de cada mes, a las 01:00:00"
    );
    assert_eq!(
        one(
            "4",
            "1",
            "2",
            "0",
            (Some("01:00:00"), None),
            (Some("2026-01-01"), None)
        ),
        "El primer lunes de cada mes, a las 01:00:00"
    );
    // Agent's kinds of day: any, a weekday, a weekend day.
    let monthly = |t, sub| {
        one(
            t,
            "1",
            sub,
            "0",
            (Some("01:00:00"), None),
            (Some("2026-01-01"), None),
        )
    };
    assert_eq!(
        monthly("4", "8"),
        "El primer día de cada mes, a las 01:00:00"
    );
    assert_eq!(
        monthly("4", "9"),
        "El primer día hábil de cada mes, a las 01:00:00"
    );
    assert_eq!(
        monthly("8", "10"),
        "El último día de fin de semana de cada mes, a las 01:00:00"
    );
    assert_eq!(
        one(
            "5",
            "3",
            "9",
            "0",
            (Some("01:00:00"), None),
            (Some("2026-01-01"), None)
        ),
        "El segundo día hábil, cada 3 meses, a las 01:00:00"
    );
    assert_eq!(monthly("6", "12"), "Tipo 6, a las 01:00:00");
    assert_eq!(monthly("6", "0"), "Tipo 6, a las 01:00:00");
}

#[test]
fn dameng_lists_jobs_with_their_runs() {
    let mut f = fake(ALL, "SYSDBA", &["DBA", "PUBLIC"]);
    let l = list(dm(), &mut f, &ctx("")).unwrap();
    assert!(l.notes.is_empty(), "{:?}", l.notes);
    let by = |id: &str| l.jobs.iter().find(|j| j.id == id).unwrap();
    let t = by("1680153662");
    assert_eq!(t.name, "TEST");
    assert_eq!(t.kind, "Job");
    assert_eq!(t.owner.as_deref(), Some("SYSDBA"));
    assert_eq!(t.description.as_deref(), Some("Every test"));
    assert_eq!(
        t.schedule.as_deref(),
        Some("Todas las semanas: martes y sábado, a las 03:00:00; Todos los días, cada 15 minutos entre las 08:00:00 y las 18:00:00, hasta el 2026-12-31")
    );
    assert!(t.enabled && t.running && t.can_stop && t.can_run && t.can_enable);
    assert!(t.has_history && t.has_script);
    assert_eq!(t.last_outcome, Some(JobOutcome::Succeeded));
    assert_eq!(t.last_started.as_deref(), Some("2026-11-12T06:00:00Z"));
    assert_eq!(t.last_duration_ms, Some(2000));
    let d = by("1680153663");
    assert_eq!(d.kind, "DBMS_JOB");
    assert_eq!(d.schedule.as_deref(), Some("Cada hora"));
    assert_eq!(d.next_run.as_deref(), Some("2026-11-12T13:00:00Z"));
    assert_eq!(d.last_outcome, Some(JobOutcome::Failed));
    assert!(!d.running && !d.can_stop);
    let h = by("1680153664");
    assert!(!h.enabled && !h.can_run, "incomplete");
    assert_eq!(h.schedule, None);

    // Not a DBA: only their own jobs can be run, stopped or enabled.
    let mut f = fake(ALL, "APP", &["PUBLIC"]);
    let l = list(dm(), &mut f, &ctx("")).unwrap();
    let by = |id: &str| l.jobs.iter().find(|j| j.id == id).unwrap();
    assert!(!by("1680153662").can_enable && !by("1680153662").can_stop);
    assert!(by("1680153663").can_enable && by("1680153663").can_run);
}

#[test]
fn dameng_without_the_tables() {
    // No SOI role: the user's views, said in a note.
    let mut f = fake(OWN, "APP", &[]);
    let l = list(dm(), &mut f, &ctx("")).unwrap();
    assert_eq!(l.jobs.len(), 3);
    assert!(
        l.notes[0].contains("solo los jobs de este usuario"),
        "{:?}",
        l.notes
    );
    assert!(f.sent("SYSJOB.USER_JOBSCHEDULES_VIEW") && f.sent("FROM USER_JOBS"));
    let r = runs(dm(), &mut f, "1680153662", 5).unwrap();
    assert_eq!(r.len(), 3);
    assert!(f.sent("SYSJOB.USER_JOBHISTORIES_VIEW WHERE NAME = ?"));

    // The job system was never created, or the user can't see it.
    for (n, want) in [("0", "SP_INIT_JOB_SYS(1)"), ("1", "permisos")] {
        let mut f = Fake::default()
            .fail(key(format!("FROM {} ORDER BY NAME", ALL.jobs)), None)
            .fail(key(format!("FROM {} ORDER BY NAME", OWN.jobs)), None)
            .rows(SCHEMA, &["N"], vec![vec![Some(n)]]);
        let l = list(dm(), &mut f, &ctx("")).unwrap();
        assert!(l.jobs.is_empty());
        assert!(l.notes[0].contains(want), "{:?}", l.notes);
    }
    let mut f = Fake::default()
        .fail(key(format!("FROM {} ORDER BY NAME", ALL.jobs)), None)
        .fail(key(format!("FROM {} ORDER BY NAME", OWN.jobs)), None)
        .fail(SCHEMA, None);
    assert!(list(dm(), &mut f, &ctx("")).is_err());
}

#[test]
fn dameng_runs_with_their_steps() {
    let mut f = fake(ALL, "SYSDBA", &["DBA"]);
    let r = runs(dm(), &mut f, "1680153662", 5).unwrap();
    assert!(f.sent(&runs_sql(&ALL, 5)) && f.sent(&step_runs_sql(&ALL)));
    assert!(f.sent("ORDER BY EXEC_ID DESC LIMIT 5"));
    let sent = |needle: &str| {
        f.log
            .iter()
            .find(|(s, _)| s.contains(needle))
            .unwrap()
            .1
            .clone()
    };
    assert_eq!(
        sent("SYSJOBHISTORIES2 WHERE NAME"),
        vec!["TEST".to_string()]
    );
    assert_eq!(
        sent("SYSSTEPHISTORIES2 WHERE NAME"),
        vec!["TEST".to_string(), "880".to_string()]
    );
    assert_eq!(r.len(), 3);
    assert_eq!(r[0].outcome, JobOutcome::Running);
    assert_eq!(r[0].started.as_deref(), Some("2026-11-12T11:59:00Z"));
    assert_eq!(r[1].id.as_deref(), Some("900"));
    assert_eq!(r[1].outcome, JobOutcome::Succeeded);
    assert_eq!(r[1].duration_ms, Some(2000));
    assert_eq!(r[1].steps.len(), 1);
    assert_eq!(r[2].id.as_deref(), Some("880"));
    assert_eq!(r[2].outcome, JobOutcome::Failed);
    assert_eq!(r[2].message.as_deref(), Some("Código de error -6102"));
    assert_eq!(r[2].steps[0].name, "STEP1");
    assert_eq!(r[2].steps[0].outcome, JobOutcome::Failed);
    assert_eq!(
        r[2].steps[0].message.as_deref(),
        Some("Reintento 2: 违反唯一性约束")
    );
    assert_eq!(r[2].steps[0].duration_ms, Some(3000));
    // The limit is capped and 0 asks nothing.
    let mut f = fake(ALL, "SYSDBA", &["DBA"]);
    runs(dm(), &mut f, "1680153662", 100_000).unwrap();
    assert!(f.sent("LIMIT 500"));
    let mut f = fake(ALL, "SYSDBA", &["DBA"]);
    assert!(runs(dm(), &mut f, "1680153662", 0).unwrap().is_empty());
    assert!(f.log.is_empty());
}

#[test]
fn dameng_controls() {
    let mut f = fake(ALL, "SYSDBA", &["DBA"]);
    run(dm(), &mut f, &ctx(""), "1680153662").unwrap();
    assert_eq!(
        f.log[0].1,
        vec!["1680153662".to_string()],
        "the id is bound"
    );
    assert_eq!(f.last().0, "CALL SP_DBMS_JOB_RUN_ASYNC(1680153662)");
    stop(dm(), &mut f, "1680153662").unwrap();
    assert_eq!(f.last().0, "CALL SP_STOP_RUNNING_JOB(1680153662)");
    set_enabled(dm(), &mut f, "1680153662", false).unwrap();
    assert_eq!(f.last().0, "CALL SP_ENABLE_JOB('TEST', 0)");
    set_enabled(dm(), &mut f, "1680153662", true).unwrap();
    assert_eq!(f.last().0, "CALL SP_ENABLE_JOB('TEST', 1)");

    // Someone else's job, for a user who isn't DBA: nothing is sent.
    let mut f = fake(ALL, "APP", &["PUBLIC"]);
    for e in [
        run(dm(), &mut f, &ctx(""), "1680153662").unwrap_err(),
        stop(dm(), &mut f, "1680153662").unwrap_err(),
        set_enabled(dm(), &mut f, "1680153662", true).unwrap_err(),
    ] {
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }
    assert!(!f.sent("CALL "));

    let mut f = fake(ALL, "SYSDBA", &["DBA"]);
    for id in FORGED {
        forged(run(dm(), &mut f, &ctx(""), id).unwrap_err());
        forged(stop(dm(), &mut f, id).unwrap_err());
        forged(set_enabled(dm(), &mut f, id, true).unwrap_err());
        forged(runs(dm(), &mut f, id, 5).unwrap_err());
        forged(detail(dm(), &mut f, &ctx(""), id).unwrap_err());
        forged(script(dm(), &mut f, id).unwrap_err());
    }
    assert!(f.log.is_empty());
}

#[test]
fn dameng_detail_and_scripts() {
    let mut f = fake(ALL, "SYSDBA", &["DBA"]);
    let d = detail(dm(), &mut f, &ctx(""), "1680153662").unwrap();
    assert_eq!(d.steps.len(), 2);
    assert_eq!(d.steps[0].kind.as_deref(), Some("SQL"));
    assert_eq!(d.steps[0].target.as_deref(), Some("DAMENG"));
    assert_eq!(d.steps[1].kind.as_deref(), Some("Actualizar estadísticas"));
    assert_eq!(d.schedules.len(), 3);
    assert!(d.schedules[2].starts_with("S3: El último viernes"));
    let sc = script(dm(), &mut f, "1680153662").unwrap();
    assert_eq!(
        sc,
        "SP_CREATE_JOB('TEST', 1, 1, 'TOM', 2, 0, '', 0, 'Every test');
SP_JOB_CONFIG_START('TEST');
SP_ADD_JOB_STEP_EX('TEST', 'STEP1', 0, 'insert into myinfo values(1000, ''Hello World'');', 3, 1, 2, 1, '/dm/log/step1.log', 0, 'first');
SP_ADD_JOB_STEP_EX('TEST', 'STEP2', 3, NULL, 1, 1, 0, 0, NULL, 0, '');
SP_JOB_STEP_SET_NEXT_STEP('TEST', 'STEP1', 'STEP2', NULL);
SP_ADD_JOB_SCHEDULE('TEST', 'S1', 1, 2, 1, 68, 0, '03:00:00', NULL, '2026-01-01', NULL, 'weekly');
SP_ADD_JOB_SCHEDULE('TEST', 'S2', 1, 1, 1, 0, 15, '08:00:00', '18:00:00', '2026-01-01', '2026-12-31', '');
SP_ADD_JOB_SCHEDULE('TEST', 'S3', 0, 8, 2, 6, 0, '23:00:00', NULL, '2026-01-01 00:00:00', NULL, '');
SP_JOB_CONFIG_COMMIT('TEST');
"
    );

    // DBMS_JOB's: ISUBMIT with its interval back as a date expression.
    let f2 = Fake::default()
        .rows(
            key(format!("FROM {} WHERE ID = ?", ALL.jobs)),
            JOB_COLS,
            vec![dbms_job()],
        )
        .rows(
            key(format!("FROM {}", ALL.dbms_jobs)),
            DBMS_JOB_COLS,
            dbms_jobs(),
        );
    let mut f2 = f2;
    let sc = script(dm(), &mut f2, "1680153663").unwrap();
    assert_eq!(
        sc,
        "CALL DBMS_JOB.ISUBMIT(1680153663, 'TEST_PROC;', CAST('2026-11-12 10:00:00' AS DATETIME), 'SYSDATE + 3600/86400');\n"
    );
    let d = detail(dm(), &mut f2, &ctx(""), "1680153663").unwrap();
    assert_eq!(d.steps[0].command.as_deref(), Some("TEST_PROC;"));
    assert_eq!(d.job.kind, "DBMS_JOB");
}

/// [`fake`] where the job looked up by id is the DBMS_JOB one, with
/// `enable` in SYSJOBS and `broken` and `next` in DBA_JOBS.
fn dbms_job_fake(enable: &'static str, broken: &'static str, next: &'static str) -> Fake {
    let mut j = dbms_job();
    j[2] = Some(enable);
    let mut d = dbms_jobs();
    d[0][3] = Some(next);
    d[0][4] = Some(broken);
    fake(ALL, "SYSDBA", &["DBA"])
        .before(
            key(format!("FROM {} WHERE ID = ?", ALL.jobs)),
            Reply::Rows(Set::new(JOB_COLS, vec![j.clone()])),
        )
        .before(
            key(format!("FROM {} ORDER BY NAME", ALL.jobs)),
            Reply::Rows(Set::new(JOB_COLS, vec![j])),
        )
        .before(
            key(format!("FROM {}", ALL.dbms_jobs)),
            Reply::Rows(Set::new(DBMS_JOB_COLS, d)),
        )
}

#[test]
fn dameng_enabling_a_broken_dbms_job_takes_it_out_of_broken() {
    let id = "1680153663";
    let ahead = "2026-11-12 10:00:00";
    let listed = |f: &mut Fake| list(dm(), f, &ctx("")).unwrap().jobs.remove(0);
    // Broken: listed off, and enabling is offered.
    let mut f = dbms_job_fake("1", "Y", ahead);
    let j = listed(&mut f);
    assert!(!j.enabled && j.can_enable, "{j:?}");
    set_enabled(dm(), &mut f, id, true).unwrap();
    assert!(f.sent("CALL SP_ENABLE_JOB('DBMSJ', 1)"));
    // Its next date is ahead: it keeps it rather than running now.
    assert_eq!(
        f.last().0,
        "CALL DBMS_JOB.BROKEN(1680153663, FALSE, CAST('2026-11-12 10:00:00' AS DATETIME))"
    );
    assert_eq!(
        f.log
            .iter()
            .find(|(s, _)| s.contains("FROM DBA_JOBS WHERE JOB = ?"))
            .unwrap()
            .1,
        vec![id.to_string()],
        "bound"
    );
    // Once those calls are in, the list has it on.
    let mut f = dbms_job_fake("1", "N", ahead);
    assert!(listed(&mut f).enabled);

    // A next date that passed, or the far-off one of a broken job: one
    // interval from now.
    for next in ["2026-11-12 08:00:00", "4000-01-01 00:00:00"] {
        let mut f = dbms_job_fake("0", "Y", next);
        set_enabled(dm(), &mut f, id, true).unwrap();
        assert!(f.sent("CALL SP_ENABLE_JOB('DBMSJ', 1)"));
        assert_eq!(
            f.last().0,
            "CALL DBMS_JOB.BROKEN(1680153663, FALSE, SYSDATE + 3600/86400)"
        );
    }
    // An interval that's an expression never goes into the call.
    let mut j = dbms_job();
    j[2] = Some("1");
    let mut d = dbms_jobs();
    d[0][3] = Some("2026-11-12 08:00:00");
    d[0][4] = Some("Y");
    d[0][5] = Some("SYSDATE + 1); DROP TABLE x; --");
    let mut f = fake(ALL, "SYSDBA", &["DBA"])
        .before(
            key(format!("FROM {} WHERE ID = ?", ALL.jobs)),
            Reply::Rows(Set::new(JOB_COLS, vec![j])),
        )
        .before(
            key(format!("FROM {}", ALL.dbms_jobs)),
            Reply::Rows(Set::new(DBMS_JOB_COLS, d)),
        );
    set_enabled(dm(), &mut f, id, true).unwrap();
    assert_eq!(f.last().0, "CALL DBMS_JOB.BROKEN(1680153663, FALSE)");
    assert!(!f.sent("DROP TABLE"));

    // Not broken: the job system's flag alone. Disabling never asks BROKEN.
    let mut f = dbms_job_fake("0", "N", ahead);
    set_enabled(dm(), &mut f, id, true).unwrap();
    assert_eq!(f.last().0, "CALL SP_ENABLE_JOB('DBMSJ', 1)");
    let mut f = dbms_job_fake("1", "Y", ahead);
    set_enabled(dm(), &mut f, id, false).unwrap();
    assert_eq!(f.last().0, "CALL SP_ENABLE_JOB('DBMSJ', 0)");
    assert!(!f.sent("DBMS_JOB.BROKEN") && !f.sent("FROM DBA_JOBS WHERE JOB"));
    // A job of the job system's own (SYS_FLAG 0) never looks in DBA_JOBS.
    let mut f = fake(ALL, "SYSDBA", &["DBA"]);
    set_enabled(dm(), &mut f, "1680153662", true).unwrap();
    assert!(!f.sent("DBMS_JOB.BROKEN") && !f.sent("FROM DBA_JOBS WHERE JOB"));
}

#[test]
fn dameng_dbms_job_script_keeps_it_off() {
    // Disabled in the job system: BROKEN brings it back off, by number.
    for (enable, broken, off) in [("0", "N", true), ("1", "Y", true), ("1", "N", false)] {
        let mut f = dbms_job_fake(enable, broken, "2026-11-12 10:00:00");
        let sc = script(dm(), &mut f, "1680153663").unwrap();
        assert!(
            sc.starts_with("CALL DBMS_JOB.ISUBMIT(1680153663, 'TEST_PROC;', "),
            "{sc}"
        );
        assert_eq!(
            sc.ends_with("CALL DBMS_JOB.BROKEN(1680153663, TRUE);\n"),
            off,
            "{enable}/{broken}: {sc}"
        );
    }
}

#[test]
fn dameng_run_and_stop_only_where_the_server_has_them() {
    let names = |list: &[&'static str]| list.iter().map(|n| vec![Some(*n)]).collect::<Vec<_>>();
    // An older build: SP_DBMS_JOB_RUN without the other two.
    let mut f = fake(ALL, "SYSDBA", &["DBA"]).rows(PROCS, &["NAME"], names(&["SP_DBMS_JOB_RUN"]));
    let l = list(dm(), &mut f, &ctx("")).unwrap();
    let t = l.jobs.iter().find(|j| j.id == "1680153662").unwrap();
    assert!(t.running && !t.can_stop && !t.can_run && t.can_enable);
    for e in [
        run(dm(), &mut f, &ctx(""), "1680153662").unwrap_err(),
        stop(dm(), &mut f, "1680153662").unwrap_err(),
    ] {
        assert!(
            matches!(&e, Error::Unsupported(m) if m.contains("esta versión de DM")),
            "{e:?}"
        );
    }
    assert!(!f.sent("CALL "));

    // All three, or a catalog that lists none of them (it can't tell):
    // both offered.
    for procs in [
        names(&[
            "SP_DBMS_JOB_RUN",
            "SP_DBMS_JOB_RUN_ASYNC",
            "SP_STOP_RUNNING_JOB",
        ]),
        Vec::new(),
    ] {
        let mut f = fake(ALL, "SYSDBA", &["DBA"]).rows(PROCS, &["NAME"], procs);
        let l = list(dm(), &mut f, &ctx("")).unwrap();
        let t = l.jobs.iter().find(|j| j.id == "1680153662").unwrap();
        assert!(t.can_run && t.can_stop);
        run(dm(), &mut f, &ctx(""), "1680153662").unwrap();
        assert_eq!(f.last().0, "CALL SP_DBMS_JOB_RUN_ASYNC(1680153662)");
    }
    // Only the async run.
    let mut f = fake(ALL, "SYSDBA", &["DBA"]).rows(
        PROCS,
        &["NAME"],
        names(&["SP_DBMS_JOB_RUN", "SP_DBMS_JOB_RUN_ASYNC"]),
    );
    run(dm(), &mut f, &ctx(""), "1680153662").unwrap();
    assert!(stop(dm(), &mut f, "1680153662").is_err());
    assert!(!f.sent("SP_STOP_RUNNING_JOB("));
}

// ------------------------------------------------------ DBMS_SCHEDULER

const SCHED_COLS: &[&str] = &[
    "OWNER",
    "JOB_NAME",
    "JOB_CREATOR",
    "PROGRAM_OWNER",
    "PROGRAM_NAME",
    "JOB_TYPE",
    "JOB_ACTION",
    "NUMBER_OF_ARGUMENTS",
    "SCHEDULE_OWNER",
    "SCHEDULE_NAME",
    "START_DATE",
    "REPEAT_INTERVAL",
    "END_DATE",
    "ENABLED",
    "AUTO_DROP",
    "STATE",
    "NEXT_RUN_DATE",
    "COMMENTS",
];

fn nightly() -> Vec<Option<&'static str>> {
    vec![
        Some("APP"),
        Some("NIGHTLY"),
        Some("APP"),
        None,
        None,
        Some("PLSQL_BLOCK"),
        Some("BEGIN x; END;"),
        Some("0"),
        None,
        None,
        Some("2026-01-01 03:00:00.000000 +08:00"),
        Some("FREQ=DAILY;BYHOUR=3"),
        None,
        Some("TRUE"),
        Some("FALSE"),
        Some("SCHEDULED"),
        Some("2026-11-13 03:00:00.000000 +08:00"),
        Some("nightly"),
    ]
}

fn lower() -> Vec<Option<&'static str>> {
    vec![
        Some("APP"),
        Some("lower_case"),
        Some("APP"),
        Some("APP"),
        Some("P1"),
        None,
        None,
        Some("0"),
        Some("SYS"),
        Some("S1"),
        None,
        None,
        None,
        Some("FALSE"),
        Some("FALSE"),
        Some("DISABLED"),
        None,
        None,
    ]
}

fn with_args() -> Vec<Option<&'static str>> {
    vec![
        Some("SYS"),
        Some("ARGS"),
        Some("SYSDBA"),
        None,
        None,
        Some("STORED_PROCEDURE"),
        Some("SYS.P2"),
        Some("2"),
        None,
        None,
        Some("2026-01-01 00:00:00"),
        Some("FREQ=HOURLY"),
        None,
        Some("TRUE"),
        Some("TRUE"),
        Some("RUNNING"),
        None,
        None,
    ]
}

/// The job system's jobs plus DBMS_SCHEDULER's, `one` the job a lookup
/// by name finds.
fn sched_fake(user: &'static str, roles: &[&'static str], one: Vec<Option<&'static str>>) -> Fake {
    fake(ALL, user, roles)
        .rows(
            "FROM DBA_SCHEDULER_JOBS WHERE OWNER = ? AND JOB_NAME = ?",
            SCHED_COLS,
            vec![one],
        )
        .rows(
            "FROM DBA_SCHEDULER_JOBS ORDER BY JOB_NAME",
            SCHED_COLS,
            vec![nightly(), lower(), with_args()],
        )
        .rows(
            "FROM DBA_SCHEDULER_JOB_RUN_DETAILS",
            &[
                "LOG_ID",
                "LOG_DATE",
                "OWNER",
                "JOB_NAME",
                "STATUS",
                "ERROR#",
                "ACTUAL_START_DATE",
                "ADDITIONAL_INFO",
            ],
            vec![
                vec![
                    Some("10"),
                    Some("2026-11-12 03:00:05.000000 +08:00"),
                    Some("APP"),
                    Some("NIGHTLY"),
                    Some("SUCCEEDED"),
                    Some("0"),
                    Some("2026-11-12 03:00:00.000000 +08:00"),
                    None,
                ],
                vec![
                    Some("9"),
                    Some("2026-11-11 03:00:01.000000 +08:00"),
                    Some("APP"),
                    Some("NIGHTLY"),
                    Some("FAILED"),
                    Some("-2106"),
                    Some("2026-11-11 03:00:00.000000 +08:00"),
                    None,
                ],
            ],
        )
        .rows(
            "FROM DBA_SCHEDULER_RUNNING_JOBS",
            &["OWNER", "JOB_NAME", "SESSION_ID"],
            vec![vec![Some("SYS"), Some("ARGS"), Some("77")]],
        )
}

#[test]
fn dameng_lists_dbms_scheduler_jobs_too() {
    let mut f = sched_fake("SYSDBA", &["DBA"], nightly());
    let l = list(dm(), &mut f, &ctx("")).unwrap();
    assert_eq!(l.jobs.len(), 6, "three of each");
    let by = |n: &str| l.jobs.iter().find(|j| j.name == n).unwrap();
    let n = by("NIGHTLY");
    assert_eq!(n.id, "\"APP\".\"NIGHTLY\"");
    assert_eq!(n.kind, "DBMS_SCHEDULER");
    assert_eq!(n.group.as_deref(), Some("APP"));
    assert_eq!(n.schedule.as_deref(), Some("FREQ=DAILY;BYHOUR=3"));
    assert_eq!(n.next_run.as_deref(), Some("2026-11-12T19:00:00Z"));
    assert_eq!(n.last_started.as_deref(), Some("2026-11-11T19:00:00Z"));
    assert_eq!(n.last_duration_ms, Some(5000));
    // The newest of the history (the fake answers every row; the newest
    // per job comes first).
    assert_eq!(n.last_outcome, Some(JobOutcome::Succeeded));
    assert!(n.enabled && n.can_run && n.can_enable && n.has_history && n.has_script);
    let lc = by("lower_case");
    assert_eq!(lc.schedule.as_deref(), Some("SYS.S1"));
    assert!(!lc.enabled && !lc.can_run && lc.next_run.is_none());
    let a = by("ARGS");
    assert!(a.running && a.can_stop && !a.has_script);
    assert_eq!(a.last_outcome, Some(JobOutcome::Running));

    // Not a DBA: only APP's are theirs.
    let mut f = sched_fake("APP", &[], nightly());
    let l = list(dm(), &mut f, &ctx("")).unwrap();
    let by = |n: &str| l.jobs.iter().find(|j| j.name == n).unwrap();
    assert!(by("NIGHTLY").can_enable && !by("ARGS").can_enable && !by("ARGS").can_stop);
}

#[test]
fn dameng_dbms_scheduler_says_when_only_own_jobs_show() {
    let note = "De DBMS_SCHEDULER se ven solo los jobs de este usuario.".to_string();
    // DBA_ refused, USER_ read: said even when the user has none.
    for own in [Vec::new(), vec![nightly()]] {
        let n = own.len();
        let mut f = fake(ALL, "APP", &[])
            .fail("FROM DBA_SCHEDULER_JOBS", None)
            .rows("FROM USER_SCHEDULER_JOBS", SCHED_COLS, own);
        let l = list(dm(), &mut f, &ctx("")).unwrap();
        assert!(l.notes.contains(&note), "{:?}", l.notes);
        assert_eq!(
            l.jobs.iter().filter(|j| j.kind == "DBMS_SCHEDULER").count(),
            n
        );
    }
    // Not created (both refused) or read whole: nothing about it.
    let mut f = fake(ALL, "APP", &[])
        .fail("FROM DBA_SCHEDULER_JOBS", None)
        .fail("FROM USER_SCHEDULER_JOBS", None);
    assert!(list(dm(), &mut f, &ctx("")).unwrap().notes.is_empty());
    let mut f = sched_fake("SYSDBA", &["DBA"], nightly());
    assert!(list(dm(), &mut f, &ctx("")).unwrap().notes.is_empty());
}

#[test]
fn dameng_dbms_scheduler_controls_and_runs() {
    let id = "\"APP\".\"NIGHTLY\"";
    let mut f = sched_fake("SYSDBA", &["DBA"], nightly());
    run(dm(), &mut f, &ctx(""), id).unwrap();
    assert_eq!(f.log[0].1, vec!["APP".to_string(), "NIGHTLY".to_string()]);
    assert_eq!(
        f.last().0,
        "CALL DBMS_SCHEDULER.RUN_JOB('APP.NIGHTLY', FALSE)"
    );
    stop(dm(), &mut f, id).unwrap();
    assert_eq!(f.last().0, "CALL DBMS_SCHEDULER.STOP_JOB('APP.NIGHTLY')");
    set_enabled(dm(), &mut f, id, false).unwrap();
    assert_eq!(f.last().0, "CALL DBMS_SCHEDULER.DISABLE('APP.NIGHTLY')");
    set_enabled(dm(), &mut f, id, true).unwrap();
    assert_eq!(f.last().0, "CALL DBMS_SCHEDULER.ENABLE('APP.NIGHTLY')");
    // A name in lower case goes quoted.
    let mut f = sched_fake("SYSDBA", &["DBA"], lower());
    set_enabled(dm(), &mut f, "\"APP\".\"lower_case\"", true).unwrap();
    assert_eq!(
        f.last().0,
        "CALL DBMS_SCHEDULER.ENABLE('\"APP\".\"lower_case\"')"
    );

    let mut f = sched_fake("SYSDBA", &["DBA"], nightly());
    let r = runs(dm(), &mut f, id, 5).unwrap();
    assert!(f.sent("FROM DBA_SCHEDULER_JOB_RUN_DETAILS WHERE OWNER = ? AND JOB_NAME = ? ORDER BY LOG_ID DESC LIMIT 5"));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].id.as_deref(), Some("10"));
    assert_eq!(r[0].started.as_deref(), Some("2026-11-11T19:00:00Z"));
    assert_eq!(r[1].outcome, JobOutcome::Failed);
    assert_eq!(r[1].message.as_deref(), Some("Código de error -2106"));
    let mut f = sched_fake("SYSDBA", &["DBA"], with_args());
    let r = runs(dm(), &mut f, "\"SYS\".\"ARGS\"", 5).unwrap();
    assert_eq!(r[0].outcome, JobOutcome::Running);

    // Someone else's, for a user who isn't DBA.
    let mut f = sched_fake("APP", &[], with_args());
    let e = stop(dm(), &mut f, "\"SYS\".\"ARGS\"").unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    assert!(!f.sent("CALL "));

    let mut f = sched_fake("SYSDBA", &["DBA"], nightly());
    for bad in [
        "\"APP\".NIGHTLY",
        "\"APP\"",
        "\"APP\".\"NIGHTLY\"; DROP",
        "\"APP\".\"N\".\"X\"",
        "\"\".\"N\"",
    ] {
        forged(run(dm(), &mut f, &ctx(""), bad).unwrap_err());
        forged(detail(dm(), &mut f, &ctx(""), bad).unwrap_err());
        forged(script(dm(), &mut f, bad).unwrap_err());
        forged(runs(dm(), &mut f, bad, 5).unwrap_err());
    }
    assert!(f.log.is_empty());
}

#[test]
fn dameng_dbms_scheduler_detail_and_script() {
    let id = "\"APP\".\"NIGHTLY\"";
    let mut f = sched_fake("SYSDBA", &["DBA"], nightly());
    let d = detail(dm(), &mut f, &ctx(""), id).unwrap();
    assert_eq!(d.steps[0].kind.as_deref(), Some("PLSQL_BLOCK"));
    assert_eq!(d.steps[0].command.as_deref(), Some("BEGIN x; END;"));
    let sc = script(dm(), &mut f, id).unwrap();
    assert_eq!(
        sc,
        "BEGIN
  DBMS_SCHEDULER.CREATE_JOB(
    JOB_NAME => 'APP.NIGHTLY',
    JOB_TYPE => 'PLSQL_BLOCK',
    JOB_ACTION => 'BEGIN x; END;',
    START_DATE => TIMESTAMP '2026-01-01 03:00:00 +08:00',
    REPEAT_INTERVAL => 'FREQ=DAILY;BYHOUR=3',
    ENABLED => TRUE,
    AUTO_DROP => FALSE,
    COMMENTS => 'nightly'
  );
END;
/
"
    );
    let mut f = sched_fake("SYSDBA", &["DBA"], lower());
    let sc = script(dm(), &mut f, "\"APP\".\"lower_case\"").unwrap();
    assert_eq!(
        sc,
        "BEGIN
  DBMS_SCHEDULER.CREATE_JOB(
    JOB_NAME => '\"APP\".\"lower_case\"',
    PROGRAM_NAME => 'APP.P1',
    SCHEDULE_NAME => 'SYS.S1',
    ENABLED => FALSE,
    AUTO_DROP => FALSE
  );
END;
/
"
    );
    let d = detail(dm(), &mut f, &ctx(""), "\"APP\".\"lower_case\"").unwrap();
    assert_eq!(d.steps[0].command.as_deref(), Some("APP.P1"));
    let mut f = sched_fake("SYSDBA", &["DBA"], with_args());
    let e = script(dm(), &mut f, "\"SYS\".\"ARGS\"").unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
}

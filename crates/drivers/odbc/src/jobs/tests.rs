//! The jobs of every engine against canned result sets: the SQL each call
//! sends, how rows become jobs and runs, and that a forged id never
//! reaches the server.

use super::*;
use crate::presets::PRESETS;
use dbine_driver::ScriptError;

enum Reply {
    Rows(Set),
    Fail(Option<&'static str>, &'static str),
    Affected(u64),
}

/// Answers the first reply whose key is in the SQL (empty otherwise) and
/// logs every statement with its parameters.
#[derive(Default)]
struct Fake {
    replies: Vec<(&'static str, Reply)>,
    log: Vec<(String, Vec<String>)>,
}

impl Fake {
    fn on(mut self, key: &'static str, reply: Reply) -> Self {
        self.replies.push((key, reply));
        self
    }

    fn rows(self, key: &'static str, cols: &[&str], rows: Vec<Vec<Option<&str>>>) -> Self {
        self.on(key, Reply::Rows(Set::new(cols, rows)))
    }

    fn fail(self, key: &'static str, code: Option<&'static str>) -> Self {
        self.on(key, Reply::Fail(code, "falló"))
    }

    /// [`Fake::on`] ahead of the replies already set, to change one.
    fn before(mut self, key: &'static str, reply: Reply) -> Self {
        self.replies.insert(0, (key, reply));
        self
    }

    fn reply(&mut self, sql: &str, params: &[&str]) -> Result<Option<&Reply>> {
        self.log.push((
            sql.to_string(),
            params.iter().map(|p| p.to_string()).collect(),
        ));
        let r = self
            .replies
            .iter()
            .find(|(k, _)| sql.contains(k))
            .map(|(_, r)| r);
        if let Some(Reply::Fail(code, msg)) = r {
            let mut e = ScriptError::new(*msg);
            if let Some(c) = code {
                e = e.with_code(*c);
            }
            return Err(e.into());
        }
        Ok(r)
    }

    fn sent(&self, needle: &str) -> bool {
        self.log.iter().any(|(s, _)| s.contains(needle))
    }

    fn last(&self) -> &(String, Vec<String>) {
        self.log.last().expect("something was sent")
    }
}

impl JobSource for Fake {
    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Set> {
        Ok(match self.reply(sql, params)? {
            Some(Reply::Rows(s)) => s.clone(),
            _ => Set::default(),
        })
    }

    fn exec(&mut self, sql: &str, params: &[&str]) -> Result<Option<u64>> {
        Ok(match self.reply(sql, params)? {
            Some(Reply::Affected(n)) => Some(*n),
            _ => None,
        })
    }
}

fn preset(id: &str) -> &'static Preset {
    PRESETS.iter().find(|p| p.id == id).expect("preset")
}

fn ctx(db: &str) -> Ctx {
    Ctx {
        database: db.into(),
    }
}

/// The server three hours behind UTC.
const MINUS_3: (&str, &str) = ("2026-11-12 09:00:00.000000", "2026-11-12 12:00:00.000000");

fn forged(e: Error) {
    assert!(
        matches!(&e, Error::Query(m) if m.contains("no es un id de job válido")),
        "{e:?}"
    );
}

const FORGED: &[&str] = &[
    "1; DROP TABLE x",
    "1 OR 1=1",
    "'",
    "",
    "-1",
    "12a",
    "1\u{0}",
];

// ---------------------------------------------------------------- time

#[test]
fn timestamps_parse_in_the_engines_forms() {
    let want = Some(rfc3339_ms("2026-11-12T03:04:05Z"));
    for t in [
        "2026-11-12 03:04:05",
        "2026-11-12 03:04:05.000000",
        "2026-11-12-03.04.05.000000",
        "2026-11-12T03:04:05",
        " 2026-11-12 03:04:05.0 ",
    ] {
        assert_eq!(parse_ts(t), want, "{t}");
    }
    assert_eq!(
        parse_ts("2026-11-12"),
        Some(rfc3339_ms("2026-11-12T00:00:00Z"))
    );
    assert_eq!(
        parse_ts("2026-11-12 03:04:05.250").map(|v| v % 1000),
        Some(250)
    );
    for bad in [
        "",
        "Nov 12 2026",
        "2026-13-01",
        "2026-11-12 25:00:00",
        "12/11/2026",
    ] {
        assert_eq!(parse_ts(bad), None, "{bad}");
    }
    assert_eq!(parse_time("03.00.00"), Some(3 * 3600 * 1000));
    assert_eq!(parse_time("23:59"), Some((23 * 60 + 59) * 60 * 1000));
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(-1000), "1969-12-31T23:59:59Z");
    assert_eq!(
        rfc3339(rfc3339_ms("2024-02-29T23:59:59Z")),
        "2024-02-29T23:59:59Z"
    );
}

/// ms of an RFC 3339 UTC time (through the parser under test, checked
/// against a known value).
fn rfc3339_ms(s: &str) -> i64 {
    parse_ts(&s.replace('T', " ").replace('Z', "")).unwrap()
}

#[test]
fn known_epoch_values() {
    assert_eq!(parse_ts("1970-01-01 00:00:00"), Some(0));
    assert_eq!(parse_ts("2026-11-12 00:00:00"), Some(1_794_441_600_000));
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    assert_eq!(civil_from_days(11_017), (2000, 3, 1));
}

#[test]
fn local_times_turn_into_utc_with_the_servers_offset() {
    let tz = Tz::from_now(Some(MINUS_3.0), Some(MINUS_3.1));
    assert_eq!(
        tz.utc(Some("2026-11-12 00:30:00")),
        Some("2026-11-12T03:30:00Z".into())
    );
    // A few seconds between the two clocks don't matter.
    let tz = Tz::from_now(Some("2026-11-12 14:30:02"), Some("2026-11-12 12:00:00"));
    assert_eq!(
        tz.utc(Some("2026-11-12 14:30:00")),
        Some("2026-11-12T12:00:00Z".into())
    );
    // UTC now as seconds since the epoch (Informix's DBINFO('utc_current')).
    let tz = Tz::from_now(Some("2026-11-12 01:00:00"), Some("1794441600"));
    assert_eq!(
        tz.utc(Some("2026-11-12 01:00:00")),
        Some("2026-11-12T00:00:00Z".into())
    );
    // Without the server's clocks, times are taken as UTC.
    assert_eq!(
        Tz::from_now(None, None).utc(Some("2026-11-12 01:00:00")),
        Some("2026-11-12T01:00:00Z".into())
    );
    assert!(tz.past(Some("2026-11-12 00:59:59")));
    assert!(!tz.past(Some("2026-11-12 01:00:01")));
    assert_eq!(
        duration_ms(Some("2026-11-12 01:00:00"), Some("2026-11-12 01:00:01.5")),
        Some(1500)
    );
    assert_eq!(
        duration_ms(Some("2026-11-12 01:00:01"), Some("2026-11-12 01:00:00")),
        None
    );
}

#[test]
fn ids_are_digits_only() {
    assert_eq!(int_id(" 42 ").unwrap(), 42);
    for id in FORGED {
        forged(int_id(id).unwrap_err());
    }
    // The error doesn't echo a huge id whole.
    let long = "9".repeat(500);
    let Error::Query(m) = int_id(&long).unwrap_err() else {
        panic!()
    };
    assert!(m.chars().count() < 120, "{m}");
}

// -------------------------------------------------------- capabilities

#[test]
fn every_preset_has_jobs_or_says_why() {
    let with = [
        "db2",
        "db2i",
        "db2zos",
        "informix",
        "gbase8s",
        "sybase",
        "sqlanywhere",
        "iris",
        "cache",
        "hive",
        "cloudera",
        "vertica",
        "dameng",
        "altibase",
        "virtuoso",
    ];
    for p in PRESETS {
        let caps = crate::design::capabilities(p);
        assert_eq!(caps.jobs, with.contains(&p.id), "{}", p.id);
        assert_eq!(supported(p), caps.jobs, "{}", p.id);
        if !caps.jobs {
            let mut f = Fake::default();
            let e = list(p, &mut f, &ctx("")).unwrap_err();
            assert!(
                matches!(e, Error::Unsupported(ref m) if !m.is_empty()),
                "{}",
                p.id
            );
            assert!(f.log.is_empty(), "{}", p.id);
        }
    }
}

// ------------------------------------------------------------- Db2 LUW

const LUW_COLS: &[&str] = &[
    "TASKID",
    "NAME",
    "OWNER",
    "OWNERTYPE",
    "BEGIN_TIME",
    "END_TIME",
    "MAX_INVOCATIONS",
    "SCHEDULE",
    "PROCEDURE_SCHEMA",
    "PROCEDURE_NAME",
    "PROCEDURE_INPUT",
    "OPTIONS",
    "UPDATE_TIME",
    "REMARKS",
    "LAST_STATUS",
    "LAST_BEGIN",
    "LAST_END",
    "LAST_SQLCODE",
    "INVOCATIONS",
    "RUNNING_NOW",
];

fn db2_fake() -> Fake {
    use super::db2::fixtures::*;
    Fake::default()
        .rows(
            ATS_TABLES,
            &["TABNAME"],
            vec![
                vec![Some("ADMIN_TASK_LIST")],
                vec![Some("ADMIN_TASK_STATUS")],
            ],
        )
        .rows(
            "DB2_ATS_ENABLE",
            &["REG_VAR_VALUE"],
            vec![vec![Some("YES")]],
        )
        .rows(
            NOW,
            &["LOCAL_NOW", "UTC_NOW"],
            vec![vec![Some(MINUS_3.0), Some(MINUS_3.1)]],
        )
        .rows(
            "FROM SYSTOOLS.ADMIN_TASK_LIST T",
            LUW_COLS,
            vec![
                vec![
                    Some("7"),
                    Some("nightly stats"),
                    Some("DBA1"),
                    Some("U"),
                    None,
                    None,
                    None,
                    Some("0 3 * * *"),
                    Some("SYSPROC"),
                    Some("ADMIN_CMD"),
                    Some("SELECT 'RUNSTATS ON TABLE T' FROM SYSIBM.SYSDUMMY1"),
                    None,
                    Some("2026-11-01 10:00:00.000000"),
                    Some("it's nightly"),
                    Some("COMPLETE"),
                    Some("2026-11-12 03:00:00.000000"),
                    Some("2026-11-12 03:00:02.500000"),
                    Some("-204"),
                    Some("12"),
                    Some("1"),
                ],
                vec![
                    Some("8"),
                    Some("old"),
                    Some("DBA1"),
                    Some("U"),
                    None,
                    Some("2026-11-01 00:00:00.000000"),
                    Some("3"),
                    Some("*/5 * * * *"),
                    Some("APP"),
                    Some("P"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some("0"),
                ],
            ],
        )
}

#[test]
fn db2_lists_the_ats_tasks() {
    let mut f = db2_fake();
    let l = list(preset("db2"), &mut f, &ctx("SAMPLE")).unwrap();
    assert_eq!(l.jobs.len(), 2);
    let j = &l.jobs[0];
    assert_eq!(j.id, "7");
    assert_eq!(j.name, "nightly stats");
    assert_eq!(j.kind, "ADMIN_TASK");
    assert_eq!(j.owner.as_deref(), Some("DBA1"));
    assert_eq!(j.description.as_deref(), Some("it's nightly"));
    assert_eq!(j.schedule.as_deref(), Some("0 3 * * *"));
    assert!(j.enabled && j.running);
    assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(j.last_started.as_deref(), Some("2026-11-12T06:00:00Z"));
    assert_eq!(j.last_duration_ms, Some(2500));
    assert!(!j.can_run && !j.can_stop && !j.can_enable);
    assert!(j.has_history && j.has_script);
    // Ended (END_TIME passed): off.
    assert!(!l.jobs[1].enabled);
    assert_eq!(l.jobs[1].last_outcome, None);
    assert_eq!(
        l.notes,
        vec!["Db2 muestra solo las tareas que creó este usuario.".to_string()]
    );
    assert!(f.sent("ROW_NUMBER() OVER (PARTITION BY TASKID"));
}

#[test]
fn db2_without_the_scheduler_says_so() {
    let mut f = Fake::default().rows(super::db2::fixtures::ATS_TABLES, &["TABNAME"], vec![]);
    let l = list(preset("db2"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.is_empty());
    assert!(l.notes[0].contains("ADMIN_TASK_ADD"), "{:?}", l.notes);
    assert!(!f.sent("ADMIN_TASK_LIST T"));

    // The scheduler off (DB2_ATS_ENABLE unset), and the user not allowed
    // to read the view.
    let mut f = Fake::default()
        .rows(
            super::db2::fixtures::ATS_TABLES,
            &["TABNAME"],
            vec![vec![Some("ADMIN_TASK_LIST")]],
        )
        .rows("DB2_ATS_ENABLE", &["REG_VAR_VALUE"], vec![])
        .fail("FROM SYSTOOLS.ADMIN_TASK_LIST T", Some("-551"));
    let l = list(preset("db2"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.is_empty());
    assert_eq!(l.notes.len(), 2, "{:?}", l.notes);
    assert!(l.notes[0].contains("DB2_ATS_ENABLE"));
    assert!(l.notes[1].contains("no puede leer"));
    // Without ADMIN_TASK_STATUS, the list doesn't join it.
    assert!(!f.sent("ROW_NUMBER()"));

    // Other failures are errors (also when the catalog can't be read).
    let mut f = Fake::default()
        .fail(super::db2::fixtures::ATS_TABLES, Some("-551"))
        .fail("FROM SYSTOOLS.ADMIN_TASK_LIST T", Some("-911"));
    assert!(list(preset("db2"), &mut f, &ctx("")).is_err());
    assert!(f.sent("ROW_NUMBER()"));
}

#[test]
fn db2_detail_runs_and_script() {
    let mut f = db2_fake();
    let d = detail(preset("db2"), &mut f, &ctx(""), "7").unwrap();
    assert!(f.last().0.contains("WHERE T.TASKID = 7"));
    assert_eq!(d.steps.len(), 1);
    assert_eq!(
        d.steps[0].command.as_deref(),
        Some("CALL \"SYSPROC\".\"ADMIN_CMD\"")
    );
    let label = |l: &str| {
        d.properties
            .iter()
            .find(|p| p.label == l)
            .map(|p| p.value.clone())
    };
    assert_eq!(
        label("Consulta de parámetros").as_deref(),
        Some("SELECT 'RUNSTATS ON TABLE T' FROM SYSIBM.SYSDUMMY1")
    );
    assert_eq!(label("Ejecuciones").as_deref(), Some("12"));

    let mut f = db2_fake().rows(
        "FROM SYSTOOLS.ADMIN_TASK_STATUS S",
        &[
            "TASKID",
            "STATUS",
            "INVOCATION",
            "BEGIN_TIME",
            "END_TIME",
            "SQLCODE",
            "SQLSTATE",
            "SQLERRMC",
            "RC",
            "MSG",
        ],
        vec![
            vec![
                Some("7"),
                Some("COMPLETE"),
                Some("11"),
                Some("2026-11-11 03:00:00"),
                Some("2026-11-11 03:00:01"),
                Some("0"),
                None,
                None,
                Some("4"),
                None,
            ],
            vec![
                Some("7"),
                Some("COMPLETE"),
                Some("12"),
                Some("2026-11-12 03:00:00"),
                Some("2026-11-12 03:00:02"),
                Some("-204"),
                Some("42704"),
                Some("542E54FF41"),
                None,
                Some("SQL0204N  \"T\" is an undefined name."),
            ],
            vec![
                Some("7"),
                Some("NOTRUN"),
                Some("10"),
                Some("2026-11-10 03:00:00"),
                None,
                Some("-440"),
                Some("42884"),
                Some("50FF41"),
                None,
                None,
            ],
        ],
    );
    let runs = runs(preset("db2"), &mut f, "7", 20).unwrap();
    let (sql, _) = f.last();
    assert!(
        sql.contains("WHERE S.TASKID = 7")
            && sql.contains("FETCH FIRST 20 ROWS ONLY")
            && sql.contains("SQLERRM("),
        "{sql}"
    );
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].id.as_deref(), Some("12"));
    assert_eq!(runs[0].outcome, JobOutcome::Failed);
    assert_eq!(runs[0].started.as_deref(), Some("2026-11-12T06:00:00Z"));
    assert_eq!(runs[0].duration_ms, Some(2000));
    assert_eq!(
        runs[0].message.as_deref(),
        Some("SQL0204N  \"T\" is an undefined name.")
    );
    assert_eq!(runs[1].outcome, JobOutcome::Succeeded);
    assert_eq!(runs[1].message.as_deref(), Some("Código de retorno 4"));
    assert_eq!(runs[2].outcome, JobOutcome::Failed);
    // The tokens, when SQLERRM isn't there.
    assert_eq!(
        runs[2].message.as_deref(),
        Some("SQLCODE -440, SQLSTATE 42884: P, A")
    );
    assert_eq!(super::db2::fixtures::tokens("542E54FF41"), "T.T, A");

    // A server without SQLERRM: the same query without it.
    let mut f = db2_fake().fail("SQLERRM(", Some("-440"));
    runs_ok(&mut f);

    let mut f = db2_fake();
    let sc = script(preset("db2"), &mut f, "7").unwrap();
    assert_eq!(
        sc,
        "CALL SYSPROC.ADMIN_TASK_ADD(
    'nightly stats',
    NULL,
    NULL,
    NULL,
    '0 3 * * *',
    'SYSPROC',
    'ADMIN_CMD',
    'SELECT ''RUNSTATS ON TABLE T'' FROM SYSIBM.SYSDUMMY1',
    NULL,
    'it''s nightly'
);
"
    );
    let mut f = db2_fake();
    // Only task 8's row, as the server would answer.
    if let Some((_, Reply::Rows(s))) = f
        .replies
        .iter_mut()
        .find(|(k, _)| k.contains("ADMIN_TASK_LIST T"))
    {
        s.rows.remove(0);
    }
    let sc = script(preset("db2"), &mut f, "8").unwrap();
    // An end that has passed: ADMIN_TASK_ADD refuses it, so it goes as
    // NULL with a comment that keeps it.
    assert!(
        sc.starts_with("-- La tarea terminó el 2026-11-01-00.00.00.000000. ADMIN_TASK_ADD no acepta un fin pasado: se crea sin fin.\nCALL SYSPROC.ADMIN_TASK_ADD(\n    'old',\n    NULL,\n    NULL,\n    3,\n"),
        "{sc}"
    );
}

/// Task 7 of [`db2_fake`] with these begin and end times (server time).
fn db2_window(begin: Option<&'static str>, end: Option<&'static str>) -> Fake {
    let mut f = db2_fake();
    if let Some((_, Reply::Rows(s))) = f
        .replies
        .iter_mut()
        .find(|(k, _)| k.contains("ADMIN_TASK_LIST T"))
    {
        s.rows.truncate(1);
        let at = |c: &str| s.cols.iter().position(|x| x == c).unwrap();
        let (b, e) = (at("BEGIN_TIME"), at("END_TIME"));
        s.rows[0][b] = begin.map(str::to_string);
        s.rows[0][e] = end.map(str::to_string);
    }
    f
}

#[test]
fn db2_scripts_leave_out_times_that_passed() {
    // The server's now is 2026-11-12 09:00 (MINUS_3).
    let mut f = db2_window(
        Some("2026-11-01 10:00:00.000000"),
        Some("2027-01-01 00:00:00.000000"),
    );
    let sc = script(preset("db2"), &mut f, "7").unwrap();
    assert_eq!(
        sc,
        "-- Inicio original: 2026-11-01-10.00.00.000000. ADMIN_TASK_ADD no acepta un inicio pasado: sin él, la tarea corre desde ya según su calendario.
CALL SYSPROC.ADMIN_TASK_ADD(
    'nightly stats',
    NULL,
    TIMESTAMP('2027-01-01-00.00.00.000000'),
    NULL,
    '0 3 * * *',
    'SYSPROC',
    'ADMIN_CMD',
    'SELECT ''RUNSTATS ON TABLE T'' FROM SYSIBM.SYSDUMMY1',
    NULL,
    'it''s nightly'
);
"
    );
    // Both in the future: kept as they are, without comments.
    let mut f = db2_window(
        Some("2026-12-01 10:00:00.000000"),
        Some("2027-01-01 00:00:00.000000"),
    );
    let sc = script(preset("db2"), &mut f, "7").unwrap();
    assert!(
        sc.starts_with("CALL SYSPROC.ADMIN_TASK_ADD(\n    'nightly stats',\n    TIMESTAMP('2026-12-01-10.00.00.000000'),\n    TIMESTAMP('2027-01-01-00.00.00.000000'),"),
        "{sc}"
    );
    // Both passed: both NULL, two comments.
    let mut f = db2_window(
        Some("2026-10-01 10:00:00.000000"),
        Some("2026-11-01 00:00:00.000000"),
    );
    let sc = script(preset("db2"), &mut f, "7").unwrap();
    assert_eq!(
        sc.lines().filter(|l| l.starts_with("-- ")).count(),
        2,
        "{sc}"
    );
    assert!(
        sc.contains("'nightly stats',\n    NULL,\n    NULL,"),
        "{sc}"
    );
    assert!(!sc.contains("TIMESTAMP("), "{sc}");
}

fn runs_ok(f: &mut Fake) {
    let r = runs(preset("db2"), f, "7", 5);
    assert!(r.is_ok(), "{r:?}");
    let (sql, _) = f.last();
    assert!(!sql.contains("SQLERRM("), "{sql}");
}

#[test]
fn db2_refuses_forged_ids_and_has_no_controls() {
    for id in FORGED {
        let mut f = db2_fake();
        forged(detail(preset("db2"), &mut f, &ctx(""), id).unwrap_err());
        forged(runs(preset("db2"), &mut f, id, 10).unwrap_err());
        forged(script(preset("db2"), &mut f, id).unwrap_err());
        assert!(f.log.is_empty(), "{id}: {:?}", f.log);
    }
    let mut f = db2_fake();
    for e in [
        run(preset("db2"), &mut f, &ctx(""), "7").unwrap_err(),
        stop(preset("db2"), &mut f, "7").unwrap_err(),
        set_enabled(preset("db2"), &mut f, "7", false).unwrap_err(),
    ] {
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }
    assert!(f.log.is_empty());
    // A task the server doesn't return.
    let mut f = Fake::default().rows("FROM SYSTOOLS.ADMIN_TASK_LIST T", LUW_COLS, vec![]);
    assert!(matches!(
        detail(preset("db2"), &mut f, &ctx(""), "99"),
        Err(Error::Query(_))
    ));
}

// ------------------------------------------------------------ Db2 z/OS

fn zos_fake() -> Fake {
    use super::db2::fixtures::*;
    Fake::default()
        .rows(
            NOW,
            &["LOCAL_NOW", "UTC_NOW"],
            vec![vec![Some(MINUS_3.0), Some(MINUS_3.1)]],
        )
        .rows(
            "ADMIN_TASK_STATUS(20)",
            &[
                "TASK_NAME",
                "STATUS",
                "NUM_INVOCATIONS",
                "START_TIMESTAMP",
                "END_TIMESTAMP",
                "MAXRC",
                "MSG",
                "SQLCODE",
                "SYSTEM_ABENDCD",
            ],
            vec![
                vec![
                    Some("REORG"),
                    Some("COMPLETED"),
                    Some("2"),
                    Some("2026-11-12 02:00:00"),
                    Some("2026-11-12 02:10:00"),
                    Some("8"),
                    Some("DSNU010I UTILITY EXECUTION COMPLETE, HIGHEST RETURN CODE=8"),
                    None,
                    Some("000"),
                ],
                vec![
                    Some("REORG"),
                    Some("COMPLETED"),
                    Some("1"),
                    Some("2026-11-11 02:00:00"),
                    Some("2026-11-11 02:05:00"),
                    Some("0"),
                    None,
                    Some("0"),
                    None,
                ],
            ],
        )
        .rows(
            ZOS_STATUS,
            &[
                "TASK_NAME",
                "STATUS",
                "NUM_INVOCATIONS",
                "START_TIMESTAMP",
                "END_TIMESTAMP",
                "MAXRC",
                "SQLCODE",
            ],
            vec![vec![
                Some("REORG"),
                Some("COMPLETED"),
                Some("2"),
                Some("2026-11-12 02:00:00"),
                Some("2026-11-12 02:10:00"),
                Some("8"),
                None,
            ]],
        )
        .rows(
            ZOS_LIST,
            &[
                "TASK_NAME",
                "DESCRIPTION",
                "BEGIN_TIMESTAMP",
                "END_TIMESTAMP",
                "MAX_INVOCATIONS",
                "INTERVAL",
                "POINT_IN_TIME",
                "PROCEDURE_SCHEMA",
                "PROCEDURE_NAME",
                "JCL_LIBRARY",
                "JCL_MEMBER",
                "DB2_SSID",
                "CREATOR",
                "USERID",
            ],
            vec![
                vec![
                    Some("REORG"),
                    Some("weekly reorg"),
                    None,
                    None,
                    None,
                    None,
                    Some("0 2 * * 0"),
                    Some("SYSPROC"),
                    Some("DSNUTILU"),
                    None,
                    None,
                    Some("DB0A"),
                    Some("ADMIN"),
                    Some("RUNNER"),
                ],
                vec![
                    Some("BATCH"),
                    None,
                    None,
                    None,
                    Some("1"),
                    Some("60"),
                    None,
                    None,
                    None,
                    Some("SYS1.JCL"),
                    Some("NIGHT"),
                    None,
                    Some("ADMIN"),
                    None,
                ],
            ],
        )
}

#[test]
fn db2_zos_tasks_from_the_table_functions() {
    let mut f = zos_fake();
    let l = list(preset("db2zos"), &mut f, &ctx("")).unwrap();
    assert!(l.notes.is_empty(), "{:?}", l.notes);
    assert_eq!(
        l.jobs.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
        ["BATCH", "REORG"]
    );
    let reorg = &l.jobs[1];
    assert_eq!(reorg.schedule.as_deref(), Some("0 2 * * 0"));
    assert_eq!(reorg.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(reorg.last_started.as_deref(), Some("2026-11-12T05:00:00Z"));
    assert_eq!(reorg.last_duration_ms, Some(600_000));
    assert_eq!(reorg.owner.as_deref(), Some("ADMIN"));
    assert!(reorg.enabled && reorg.has_history && reorg.has_script && !reorg.can_run);
    let batch = &l.jobs[0];
    assert_eq!(batch.schedule.as_deref(), Some("INTERVAL 60"));
    assert_eq!(batch.last_outcome, None);
    // Stop only while it runs (ADMIN_TASK_CANCEL, which the session runs
    // through the scheduler's plan).
    assert!(!reorg.running && !reorg.can_stop);
    let mut f = zos_fake();
    if let Some((_, Reply::Rows(s))) = f
        .replies
        .iter_mut()
        .find(|(k, _)| *k == super::db2::fixtures::ZOS_STATUS)
    {
        s.rows[0][1] = Some("RUNNING".into());
    }
    let l = list(preset("db2zos"), &mut f, &ctx("")).unwrap();
    let reorg = &l.jobs[1];
    assert!(reorg.running && reorg.can_stop);

    let mut f = zos_fake();
    let d = detail(preset("db2zos"), &mut f, &ctx(""), "BATCH").unwrap();
    assert_eq!(d.steps[0].kind.as_deref(), Some("JCL"));
    assert_eq!(d.steps[0].command.as_deref(), Some("SYS1.JCL(NIGHT)"));

    let mut f = zos_fake();
    let r = runs(preset("db2zos"), &mut f, "REORG", 20).unwrap();
    assert_eq!(f.last().1, vec!["REORG".to_string()], "the name is bound");
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].outcome, JobOutcome::Failed);
    assert!(r[0]
        .message
        .as_deref()
        .unwrap()
        .contains("Código de retorno 8"));
    assert_eq!(r[1].outcome, JobOutcome::Succeeded);

    let mut f = zos_fake();
    let sc = script(preset("db2zos"), &mut f, "REORG").unwrap();
    assert!(
        sc.starts_with("CALL SYSPROC.ADMIN_TASK_ADD(\n    NULL,\n    NULL,"),
        "{sc}"
    );
    assert!(
        sc.contains("'0 2 * * 0'")
            && sc.contains("'REORG',\n    'weekly reorg',\n    ?,\n    ?\n);"),
        "{sc}"
    );
    assert_eq!(sc.matches(",\n").count(), 20, "21 arguments: {sc}");

    // BATCH began in the past and ends in the future (server time): only
    // the begin goes as NULL, with a comment.
    let mut f = zos_fake();
    if let Some((_, Reply::Rows(s))) = f.replies.iter_mut().find(|(k, _)| *k == ZOS_LIST_KEY) {
        s.rows[1][2] = Some("2026-11-01 00:00:00.000000".into());
        s.rows[1][3] = Some("2026-12-31 00:00:00.000000".into());
    }
    let sc = script(preset("db2zos"), &mut f, "BATCH").unwrap();
    assert!(
        sc.starts_with("-- Inicio original: 2026-11-01-00.00.00.000000. ADMIN_TASK_ADD no acepta un inicio pasado")
            && sc.contains("\nCALL SYSPROC.ADMIN_TASK_ADD(\n    NULL,\n    NULL,\n    NULL,\n    TIMESTAMP('2026-12-31-00.00.00.000000'),\n    1,\n    60,"),
        "{sc}"
    );
    // An end that passed: NULL too.
    if let Some((_, Reply::Rows(s))) = f.replies.iter_mut().find(|(k, _)| *k == ZOS_LIST_KEY) {
        s.rows[1][3] = Some("2026-11-02 00:00:00.000000".into());
    }
    let sc = script(preset("db2zos"), &mut f, "BATCH").unwrap();
    assert!(
        sc.contains("La tarea terminó el 2026-11-02-00.00.00.000000"),
        "{sc}"
    );
    assert!(!sc.contains("TIMESTAMP("), "{sc}");

    // Forged names never reach the server.
    let mut f = zos_fake();
    for id in ["", "A\u{0}B", &"X".repeat(129)] {
        forged(runs(preset("db2zos"), &mut f, id, 5).unwrap_err());
        forged(detail(preset("db2zos"), &mut f, &ctx(""), id).unwrap_err());
        forged(script(preset("db2zos"), &mut f, id).unwrap_err());
    }
    assert!(f.log.is_empty());
}

const ZOS_LIST_KEY: &str = super::db2::fixtures::ZOS_LIST;

#[test]
fn db2_zos_without_the_scheduler_or_monitor1() {
    use super::db2::fixtures::*;
    let mut f = Fake::default().fail(ZOS_LIST, Some("-440"));
    let l = list(preset("db2zos"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.is_empty() && l.notes[0].contains("DSNADM.ADMIN_TASK_LIST"));
    let mut f = Fake::default().fail(ZOS_STATUS, Some("-551")).rows(
        ZOS_LIST,
        &["TASK_NAME"],
        vec![vec![Some("A")]],
    );
    let l = list(preset("db2zos"), &mut f, &ctx("")).unwrap();
    assert_eq!(l.jobs.len(), 1);
    assert!(!l.jobs[0].has_history);
    assert!(l.notes[0].contains("MONITOR1"));
}

// ------------------------------------------------------------ Db2 for i

fn db2i_fake() -> Fake {
    Fake::default()
        .rows(
            super::db2i::fixtures::LIST,
            &[
                "SCHEDULED_JOB_NAME",
                "SCHEDULED_JOB_ENTRY_NUMBER",
                "STATUS",
                "DESCRIPTION",
                "SCHEDULED_BY",
                "FREQUENCY",
                "SCHEDULED_DATE_VALUE",
                "SCHEDULED_DATE",
                "SCHEDULED_TIME",
                "SCHEDULED_DAYS",
                "RELATIVE_DAYS_OF_MONTH",
                "NEXT_SUBMISSION_DATE",
                "DATES_OMITTED",
                "RECOVERY_ACTION",
                "JOB_QUEUE_NAME",
                "JOB_QUEUE_LIBRARY_NAME",
                "USER_PROFILE_FOR_SUBMITTED_JOB",
                "KEEP_ENTRY",
                "COMMAND_STRING",
                "LAST_SUCCESSFUL_SUBMISSION_TIMESTAMP",
                "LAST_ATTEMPTED_SUBMISSION_TIMESTAMP",
                "LAST_ATTEMPTED_SUBMISSION_STATUS",
            ],
            vec![
                vec![
                    Some("NIGHTLY"),
                    Some("123"),
                    Some("SCHEDULED"),
                    Some("Copia 'nocturna'"),
                    Some("QSECOFR"),
                    Some("*WEEKLY"),
                    Some("*NONE"),
                    None,
                    Some("03:00:00"),
                    Some("*MON *WED"),
                    None,
                    Some("2026-11-16"),
                    Some("2026-12-25"),
                    Some("*SBMRLS"),
                    Some("QBATCH"),
                    Some("QGPL"),
                    Some("BATCHUSR"),
                    Some("*YES"),
                    Some("SNDMSG MSG('Hola') TOUSR(QSYSOPR)"),
                    Some("2026-11-11 03:00:00.000000"),
                    // The last attempt, a day later, failed.
                    Some("2026-11-12 03:00:00.000000"),
                    Some("LAST JOB SUBMISSION FAILED - SEE JOB MESSAGE QUEUE"),
                ],
                vec![
                    Some("PURGE"),
                    Some("7"),
                    Some("HELD"),
                    None,
                    Some("OTHER"),
                    Some("*ONCE"),
                    None,
                    Some("2026-12-01"),
                    Some("22:30:00"),
                    None,
                    None,
                    Some("2026-12-01"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some("2026-11-10 22:30:00.000000"),
                    Some("JOB NOT SUBMITTED DUE TO HELD STATUS"),
                ],
            ],
        )
        .rows(
            "CURRENT TIMEZONE",
            &["LOCAL_NOW", "UTC_NOW"],
            vec![vec![Some(MINUS_3.0), Some(MINUS_3.1)]],
        )
}

#[test]
fn db2i_lists_job_schedule_entries() {
    let mut f = db2i_fake();
    let l = list(preset("db2i"), &mut f, &ctx("")).unwrap();
    assert_eq!(
        l.jobs.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
        ["000123/NIGHTLY", "000007/PURGE"]
    );
    let n = &l.jobs[0];
    assert!(n.enabled && n.can_enable && !n.can_run && !n.has_history && n.has_script);
    assert_eq!(
        n.schedule.as_deref(),
        Some("FRQ(*WEEKLY) SCDDATE(*NONE) SCDDAY(*MON *WED) SCDTIME(030000)")
    );
    assert_eq!(n.next_run.as_deref(), Some("2026-11-16T06:00:00Z"));
    // The last attempt (failed), not the earlier successful submission.
    assert_eq!(n.last_started.as_deref(), Some("2026-11-12T06:00:00Z"));
    assert_eq!(n.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(n.owner.as_deref(), Some("QSECOFR"));
    let p = &l.jobs[1];
    assert!(!p.enabled && p.can_enable && p.next_run.is_none());
    // Held: the scheduler skipped it.
    assert_eq!(p.last_outcome, Some(JobOutcome::Skipped));
    assert_eq!(p.last_started.as_deref(), Some("2026-11-11T01:30:00Z"));
    assert_eq!(
        super::db2i::fixtures::submission("JOB SUCCESSFULLY SUBMITTED"),
        Some(JobOutcome::Succeeded)
    );
    assert_eq!(super::db2i::fixtures::submission("?"), None);
    // An entry without its command: the user lacks *JOBCTL.
    assert!(l.notes[0].contains("*JOBCTL"));
}

#[test]
fn db2i_holds_and_releases_with_checked_names() {
    let mut f = db2i_fake();
    set_enabled(preset("db2i"), &mut f, "000123/nightly", false).unwrap();
    assert_eq!(
        f.last().0,
        "CALL QSYS2.QCMDEXC('HLDJOBSCDE JOB(NIGHTLY) ENTRYNBR(000123)')"
    );
    set_enabled(preset("db2i"), &mut f, "7/PURGE", true).unwrap();
    assert_eq!(
        f.last().0,
        "CALL QSYS2.QCMDEXC('RLSJOBSCDE JOB(PURGE) ENTRYNBR(000007)')"
    );
    let mut f = db2i_fake();
    for id in [
        "000123/X') OR (1=1",
        "000123/NIGHTLY JOB(Y)",
        "000123/TOOLONGNAME1",
        "1234567/A",
        "000123/1ABC",
        "NIGHTLY",
        "000123/",
        "/NIGHTLY",
    ] {
        forged(set_enabled(preset("db2i"), &mut f, id, true).unwrap_err());
        forged(script(preset("db2i"), &mut f, id).unwrap_err());
    }
    assert!(f.log.is_empty(), "{:?}", f.log);
    assert!(matches!(
        run(preset("db2i"), &mut f, &ctx(""), "000123/NIGHTLY"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        runs(preset("db2i"), &mut f, "000123/NIGHTLY", 5),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn db2i_detail_and_script() {
    let mut f = db2i_fake();
    let d = detail(preset("db2i"), &mut f, &ctx(""), "000123/NIGHTLY").unwrap();
    assert_eq!(
        d.steps[0].command.as_deref(),
        Some("SNDMSG MSG('Hola') TOUSR(QSYSOPR)")
    );
    assert!(d
        .properties
        .iter()
        .any(|p| p.label == "Cola de trabajos" && p.value == "QGPL/QBATCH"));
    assert!(matches!(
        detail(preset("db2i"), &mut f, &ctx(""), "000124/NIGHTLY"),
        Err(Error::Query(_))
    ));

    let mut f = db2i_fake();
    let sc = script(preset("db2i"), &mut f, "000123/NIGHTLY").unwrap();
    assert_eq!(
        sc,
        "CALL QSYS2.QCMDEXC('CHGJOB DATFMT(*YMD)');\nCALL QSYS2.QCMDEXC('ADDJOBSCDE JOB(NIGHTLY) CMD(SNDMSG MSG(''Hola'') TOUSR(QSYSOPR)) FRQ(*WEEKLY) SCDDATE(*NONE) SCDDAY(*MON *WED) SCDTIME(030000) SAVE(*YES) OMITDATE(261225) RCYACN(*SBMRLS) JOBQ(QGPL/QBATCH) USER(BATCHUSR) TEXT(''Copia ''''nocturna'''''')');\n"
    );
    let sc = script(preset("db2i"), &mut f, "000007/PURGE").unwrap();
    assert!(
        sc.contains("FRQ(*ONCE) SCDDATE(261201) SCDTIME(223000)"),
        "{sc}"
    );
    assert!(
        sc.ends_with("CALL QSYS2.QCMDEXC('HLDJOBSCDE JOB(PURGE) ENTRYNBR(*ONLY)');\n"),
        "{sc}"
    );
}

#[test]
fn db2i_without_the_view() {
    let mut f = Fake::default().fail(super::db2i::fixtures::LIST, Some("-204"));
    let l = list(preset("db2i"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.is_empty() && l.notes[0].contains("7.2 TR2"));
}

// ------------------------------------------------------------ Informix

const IFX_COLS: &[&str] = &[
    "tk_id",
    "tk_name",
    "tk_type",
    "tk_group",
    "tk_description",
    "tk_dbs",
    "tk_execute",
    "tk_start_time",
    "tk_stop_time",
    "tk_frequency",
    "tk_next_execution",
    "tk_delete",
    "tk_total_executions",
    "tk_total_time",
    "tk_monday",
    "tk_tuesday",
    "tk_wednesday",
    "tk_thursday",
    "tk_friday",
    "tk_saturday",
    "tk_sunday",
    "tk_enable",
    "tk_priority",
    "tk_result_table",
    "tk_create",
    "last_run",
    "last_retcode",
    "last_duration",
];

fn ifx_fake() -> Fake {
    use super::informix::fixtures::*;
    Fake::default()
        // One hour ahead of UTC (08:00 UTC as seconds since the epoch).
        .rows(
            NOW,
            &["LOCAL_NOW", "UTC_NOW"],
            vec![vec![Some("2026-11-12 09:00:00"), Some("1794470400")]],
        )
        .rows(ASYNC, &["N"], vec![vec![Some("1")]])
        .rows(
            "FROM sysadmin:ph_alert",
            &["alert_task_seq", "alert_message"],
            vec![vec![Some("41"), Some("Table 'x' not found")]],
        )
        .rows(
            "FROM sysadmin:ph_run WHERE",
            &[
                "run_id",
                "run_task_seq",
                "run_time",
                "run_retcode",
                "run_duration",
            ],
            vec![
                vec![
                    Some("900"),
                    Some("41"),
                    Some("2026-11-12 03:00:00"),
                    Some("-206"),
                    Some("0.25"),
                ],
                vec![
                    Some("899"),
                    Some("40"),
                    Some("2026-11-11 03:00:00"),
                    Some("0"),
                    Some("1.5"),
                ],
            ],
        )
        .rows(
            "FROM sysadmin:ph_task t",
            IFX_COLS,
            vec![
                vec![
                    Some("12"),
                    Some("purge_logs                          "),
                    Some("TASK              "),
                    Some("TABLES"),
                    Some("Purges"),
                    Some("sysadmin"),
                    Some("DELETE FROM logs WHERE d < TODAY - 7"),
                    Some("03:00:00"),
                    None,
                    Some(" 1 00:00:00"),
                    Some("2026-11-13 03:00:00"),
                    None,
                    Some("40"),
                    Some("12.5"),
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("f"),
                    Some("f"),
                    Some("t"),
                    Some("0"),
                    None,
                    None,
                    Some("2026-11-12 03:00:00"),
                    Some("-206"),
                    Some("0.25"),
                ],
                vec![
                    Some("3"),
                    Some("mon_config"),
                    Some("STARTUP SENSOR"),
                    Some("SERVER"),
                    None,
                    Some("sysadmin"),
                    Some("onconfig_save_diffs"),
                    None,
                    None,
                    None,
                    None,
                    Some(" 99 00:00:00"),
                    Some("1"),
                    Some("0.1"),
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("t"),
                    Some("f"),
                    Some("0"),
                    Some("mon_onconfig"),
                    Some("create table mon_onconfig (ID integer, name varchar(124))"),
                    None,
                    None,
                    None,
                ],
            ],
        )
}

#[test]
fn informix_lists_scheduler_tasks() {
    let mut f = ifx_fake();
    let l = list(preset("informix"), &mut f, &ctx("sysadmin")).unwrap();
    assert!(l.notes.is_empty());
    let t = &l.jobs[0];
    assert_eq!(
        (t.id.as_str(), t.name.as_str(), t.kind.as_str()),
        ("12", "purge_logs", "Task")
    );
    assert_eq!(t.group.as_deref(), Some("TABLES"));
    assert_eq!(
        t.schedule.as_deref(),
        Some("EVERY 1 00:00:00 FROM 03:00:00 ON MON,TUE,WED,THU,FRI")
    );
    // The server is one hour ahead of UTC.
    assert_eq!(t.next_run.as_deref(), Some("2026-11-13T02:00:00Z"));
    assert_eq!(t.last_started.as_deref(), Some("2026-11-12T02:00:00Z"));
    assert_eq!(t.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(t.last_duration_ms, Some(250));
    assert!(t.enabled && t.can_run && t.can_enable && t.has_history && t.has_script && !t.can_stop);
    let s = &l.jobs[1];
    assert_eq!(s.kind, "Startup sensor");
    assert_eq!(s.schedule, None);
    assert!(!s.enabled);
    assert_eq!(s.last_outcome, None);
    // mon_config is one of the Scheduler's built-in sensors; purge_logs is
    // the user's.
    assert!(s.system && !t.system);

    // From another database: no run now, and a note saying where it works.
    let mut f = ifx_fake();
    let l = list(preset("gbase8s"), &mut f, &ctx("stores")).unwrap();
    assert!(l.jobs.iter().all(|j| !j.can_run));
    assert!(l.notes[0].contains("sysadmin"));
    assert!(!f.sent("sysprocedures"));
}

#[test]
fn informix_without_access_to_sysadmin() {
    for (code, word) in [
        ("-387", "acceso"),
        ("-329", "no tiene la base"),
        ("-206", "ph_task"),
    ] {
        let mut f = Fake::default().on("FROM sysadmin:ph_task t", Reply::Fail(Some(code), "x"));
        let l = list(preset("informix"), &mut f, &ctx("stores")).unwrap();
        assert!(
            l.jobs.is_empty() && l.notes[0].contains(word),
            "{code}: {:?}",
            l.notes
        );
    }
    let mut f = Fake::default().fail("FROM sysadmin:ph_task t", Some("-111"));
    assert!(list(preset("informix"), &mut f, &ctx("")).is_err());
}

#[test]
fn informix_runs_controls_and_script() {
    let mut f = ifx_fake();
    let r = runs(preset("informix"), &mut f, "12", 30).unwrap();
    assert!(f.sent("SELECT FIRST 30 run_id") && f.sent("run_task_id = 12"));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].id.as_deref(), Some("900"));
    assert_eq!(r[0].outcome, JobOutcome::Failed);
    assert_eq!(
        r[0].message.as_deref(),
        Some("Código de retorno -206\nTable 'x' not found")
    );
    assert_eq!(r[1].outcome, JobOutcome::Succeeded);
    assert_eq!(r[1].duration_ms, Some(1500));

    let mut f = ifx_fake();
    run(preset("informix"), &mut f, &ctx("sysadmin"), "12").unwrap();
    assert_eq!(f.last().0, "EXECUTE FUNCTION exectask_async('purge_logs')");
    let mut f = ifx_fake();
    assert!(matches!(
        run(preset("informix"), &mut f, &ctx("stores"), "12"),
        Err(Error::Unsupported(_))
    ));
    assert!(f.log.is_empty());

    let mut f = ifx_fake().on("UPDATE sysadmin:ph_task", Reply::Affected(1));
    set_enabled(preset("informix"), &mut f, "12", false).unwrap();
    assert_eq!(
        f.last().0,
        "UPDATE sysadmin:ph_task SET tk_enable = 'f' WHERE tk_id = 12"
    );
    let mut f = ifx_fake().on("UPDATE sysadmin:ph_task", Reply::Affected(0));
    assert!(matches!(
        set_enabled(preset("informix"), &mut f, "13", true),
        Err(Error::Query(_))
    ));

    let mut f = ifx_fake();
    for id in FORGED {
        forged(set_enabled(preset("informix"), &mut f, id, true).unwrap_err());
        forged(run(preset("informix"), &mut f, &ctx("sysadmin"), id).unwrap_err());
        forged(runs(preset("informix"), &mut f, id, 5).unwrap_err());
    }
    assert!(f.log.is_empty(), "{:?}", f.log);

    let mut f = ifx_fake();
    let sc = script(preset("informix"), &mut f, "12").unwrap();
    assert!(
        sc.starts_with("DATABASE sysadmin;\nINSERT INTO ph_task ("),
        "{sc}"
    );
    assert!(
        sc.contains("'purge_logs'") && sc.contains("'DELETE FROM logs WHERE d < TODAY - 7'"),
        "{sc}"
    );
    assert!(
        sc.contains("DATETIME(03:00:00) HOUR TO SECOND")
            && sc.contains("INTERVAL(1 00:00:00) DAY TO SECOND"),
        "{sc}"
    );
    assert!(
        sc.contains("tk_stop_time") && sc.contains("    NULL,"),
        "{sc}"
    );
    assert!(!sc.contains("tk_result_table"));
    assert!(!sc.contains("IFX_ALLOW_NEWLINE"));
}

#[test]
fn informix_script_literals_keep_their_shape() {
    use super::informix::fixtures::{runs_sql, tasks_sql};
    assert!(
        runs_sql(1, 7).contains("SELECT FIRST 7 ") && runs_sql(1, 7).contains("run_task_id = 1")
    );
    assert_eq!(tasks_sql(Some(5)).matches("t.tk_id = 5").count(), 1);
    assert!(!tasks_sql(None).contains("WHERE"));
    // A value that isn't a time or an interval doesn't go in the literal.
    let mut f = Fake::default().rows(
        "FROM sysadmin:ph_task t",
        IFX_COLS,
        vec![{
            let mut r: Vec<Option<&str>> = vec![None; IFX_COLS.len()];
            r[0] = Some("5");
            r[1] = Some("x");
            r[6] = Some("SELECT 1\nFROM t");
            r[7] = Some("03:00:00) HOUR TO SECOND; DROP TABLE ph_task; --");
            r[9] = Some("1) DAY TO SECOND; DROP");
            r
        }],
    );
    let sc = script(preset("informix"), &mut f, "5").unwrap();
    assert!(!sc.contains("DROP"), "{sc}");
    assert!(
        sc.contains("EXECUTE PROCEDURE IFX_ALLOW_NEWLINE('t');"),
        "{sc}"
    );
}

// ----------------------------------------------------------------- ASE

fn ase_fake(admin: &'static str) -> Fake {
    use super::ase::fixtures::*;
    Fake::default()
        .rows(INSTALLED, &["DBID"], vec![vec![Some("31")]])
        .rows(ROLES, &["ADM", "USR"], vec![vec![Some(admin), Some("1")]])
        .rows(ENABLED, &["VALUE"], vec![vec![Some("1")]])
        .rows(
            NOW,
            &["LOCAL_NOW", "UTC_NOW"],
            vec![vec![Some(MINUS_3.0), Some(MINUS_3.1)]],
        )
        .rows(
            "sp_sjobhistory",
            &[
                "job_runid",
                "job_name",
                "job_state",
                "job_end",
                "job_user_code",
                "job_os_code",
                "job_user_req",
                "job_long_message",
                "sjob_id",
                "sched_name",
                "job_start",
                "job_exit_code",
                "job_atat_error",
                "job_user_run",
                "job_short_message",
                "job_size",
            ],
            vec![
                vec![
                    Some("1532"),
                    Some("load"),
                    Some("C2"),
                    Some("2026-11-12 01:00:05"),
                    None,
                    None,
                    Some("ops"),
                    Some("Msg 208: sales not found"),
                    Some("5"),
                    Some("nightly"),
                    Some("2026-11-12 01:00:00"),
                    Some("0"),
                    Some("208"),
                    Some("jsuser"),
                    Some("error"),
                    Some("10"),
                ],
                vec![
                    Some("1530"),
                    Some("load"),
                    Some("C2"),
                    Some("2026-11-11 01:00:03"),
                    None,
                    None,
                    Some("jsuser"),
                    None,
                    Some("5"),
                    Some("nightly"),
                    Some("2026-11-11 01:00:00"),
                    Some("0"),
                    Some("0"),
                    Some("jsuser"),
                    None,
                    Some("10"),
                ],
                vec![
                    Some("1520"),
                    Some("other"),
                    Some("X2"),
                    Some("2026-11-10 01:00:03"),
                    None,
                    None,
                    None,
                    None,
                    Some("6"),
                    None,
                    Some("2026-11-10 01:00:00"),
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
                // Job 6 is running now.
                vec![
                    Some("1540"),
                    Some("other"),
                    Some("R2"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some("6"),
                    None,
                    Some("2026-11-12 08:59:00"),
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
                // And another one of job 6, queued: no start yet.
                vec![
                    Some("1541"),
                    Some("other"),
                    Some("Q"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some("6"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
            ],
        )
        .rows("sp_sjobcmd", &["text"], vec![vec![Some("exec load_sales")]])
        .rows(
            "sp_sjobhelp",
            &[
                "sjob_id",
                "sjob_name",
                "sjob_owner",
                "sjob_state",
                "job_name",
                "sched_name",
                "sjob_server",
                "sched_starttime",
                "sched_repeats",
            ],
            vec![
                vec![
                    Some("5"),
                    Some("svr1_load"),
                    Some("jsuser"),
                    Some("enabled"),
                    Some("load"),
                    Some("nightly"),
                    Some("SVR1"),
                    Some("01:00"),
                    Some("1day"),
                ],
                vec![
                    Some("6"),
                    Some("svr1_other"),
                    Some("ops"),
                    Some("disabled"),
                    Some("other"),
                    None,
                    Some("SVR1"),
                    None,
                    None,
                ],
            ],
        )
}

#[test]
fn ase_lists_job_scheduler_jobs() {
    let mut f = ase_fake("1");
    let l = list(preset("sybase"), &mut f, &ctx("")).unwrap();
    assert!(f.sent("exec sybmgmtdb..sp_sjobhelp @option = 'list,scheduled,all_users'"));
    assert!(l.notes.is_empty(), "{:?}", l.notes);
    let j = &l.jobs[0];
    assert_eq!((j.id.as_str(), j.name.as_str()), ("5", "svr1_load"));
    assert!(j.enabled && j.can_run && j.can_enable && j.has_history && !j.has_script);
    // Not running: nothing to stop.
    assert!(!j.running && !j.can_stop);
    let k = &l.jobs[1];
    assert!(k.running && k.can_stop);
    assert_eq!(j.schedule.as_deref(), Some("repeats=1day,starttime=01:00"));
    assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(j.last_started.as_deref(), Some("2026-11-12T04:00:00Z"));
    assert_eq!(j.last_duration_ms, Some(5000));
    assert!(!l.jobs[1].enabled);
    assert_eq!(l.jobs[1].last_outcome, Some(JobOutcome::Canceled));

    // Without js_admin_role: own jobs only, said in a note.
    let mut f = ase_fake("0");
    let l = list(preset("sybase"), &mut f, &ctx("")).unwrap();
    assert!(f.sent("@option = 'list,scheduled'"));
    assert!(l.notes[0].contains("js_admin_role"));
}

#[test]
fn ase_without_job_scheduler() {
    use super::ase::fixtures::*;
    let mut f = Fake::default().rows(INSTALLED, &["DBID"], vec![vec![None]]);
    let l = list(preset("sybase"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.is_empty() && l.notes[0].contains("sybmgmtdb"));
    let mut f = Fake::default()
        .rows(INSTALLED, &["DBID"], vec![vec![Some("31")]])
        .rows(ROLES, &["ADM", "USR"], vec![vec![Some("0"), Some("0")]]);
    let l = list(preset("sybase"), &mut f, &ctx("")).unwrap();
    assert!(l.notes[0].contains("js_user_role"));
    assert!(!f.sent("sp_sjobhelp"));
    // Off: listed, with a note.
    let mut f = ase_fake("1").rows(ENABLED, &["VALUE"], vec![vec![Some("0")]]);
    f.replies.swap(2, 7);
    let l = list(preset("sybase"), &mut f, &ctx("")).unwrap();
    assert!(
        l.notes.iter().any(|n| n.contains("enable job scheduler")),
        "{:?}",
        l.notes
    );
    // A list whose columns aren't known: nothing invented.
    let mut f = Fake::default()
        .rows(INSTALLED, &["DBID"], vec![vec![Some("31")]])
        .rows(ROLES, &["ADM", "USR"], vec![vec![Some("1"), Some("1")]])
        .rows("sp_sjobhelp", &["what"], vec![vec![Some("x")]]);
    let l = list(preset("sybase"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.is_empty() && l.notes.iter().any(|n| n.contains("sp_sjobhelp")));
}

#[test]
fn ase_runs_and_controls() {
    let mut f = ase_fake("1");
    let r = runs(preset("sybase"), &mut f, "5", 10).unwrap();
    assert!(f.sent("exec sybmgmtdb..sp_sjobhistory @name = '5', @option = 'list,all_users'"));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].id.as_deref(), Some("1532"));
    assert_eq!(r[0].outcome, JobOutcome::Failed);
    assert_eq!(r[0].trigger.as_deref(), Some("pedida por ops"));
    assert_eq!(
        r[0].message.as_deref(),
        Some("Msg 208: sales not found\nerror\n@@error 208")
    );
    assert_eq!(r[1].outcome, JobOutcome::Succeeded);
    assert_eq!(r[1].trigger, None);
    // Newest first by run id: the queued run, not started yet, leads.
    let mut f = ase_fake("1");
    let r = runs(preset("sybase"), &mut f, "6", 10).unwrap();
    assert_eq!(
        r.iter()
            .map(|r| r.id.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["1541", "1540", "1520"]
    );
    assert!(r[0].started.is_none());

    for (call, option) in [
        ("run", "run_now"),
        ("stop", "terminate"),
        ("on", "enable"),
        ("off", "disable"),
    ] {
        let mut f = ase_fake("0");
        match call {
            "run" => run(preset("sybase"), &mut f, &ctx(""), "5").unwrap(),
            "stop" => stop(preset("sybase"), &mut f, "5").unwrap(),
            "on" => set_enabled(preset("sybase"), &mut f, "5", true).unwrap(),
            _ => set_enabled(preset("sybase"), &mut f, "5", false).unwrap(),
        }
        assert_eq!(
            f.last().0,
            format!("exec sybmgmtdb..sp_sjobcontrol @name = '5', @option = '{option}'")
        );
    }

    let mut f = ase_fake("1");
    let d = detail(preset("sybase"), &mut f, &ctx(""), "5").unwrap();
    assert!(f.sent("sp_sjobhelp @name = '5', @option = 'list,all_users'"));
    assert_eq!(d.steps[0].command.as_deref(), Some("exec load_sales"));
    assert_eq!(d.job.last_outcome, Some(JobOutcome::Failed));
    assert!(d
        .properties
        .iter()
        .any(|p| p.label == "sched_starttime" && p.value == "01:00"));
    assert!(matches!(
        script(preset("sybase"), &mut f, "5"),
        Err(Error::Unsupported(_))
    ));

    let mut f = ase_fake("1");
    for id in FORGED {
        forged(run(preset("sybase"), &mut f, &ctx(""), id).unwrap_err());
        forged(stop(preset("sybase"), &mut f, id).unwrap_err());
        forged(runs(preset("sybase"), &mut f, id, 5).unwrap_err());
    }
    assert!(f.log.is_empty(), "{:?}", f.log);
}

// --------------------------------------------------------- SQL Anywhere

fn sqla_fake() -> Fake {
    use super::sqla::fixtures::*;
    Fake::default()
        .rows(
            SCHEDULES,
            &[
                "event_id",
                "sched_name",
                "recurring",
                "start_time",
                "stop_time",
                "start_date",
                "days_of_week",
                "days_of_month",
                "interval_units",
                "interval_amt",
            ],
            vec![
                vec![
                    Some("4"),
                    Some("nightly"),
                    Some("1"),
                    Some("03:00:00.000"),
                    None,
                    Some("2026-01-01"),
                    Some("34"),
                    Some("0"),
                    None,
                    None,
                ],
                vec![
                    Some("9"),
                    Some("work"),
                    Some("1"),
                    Some("08:00:00"),
                    Some("18:00:00"),
                    None,
                    Some("0"),
                    Some("2147483649"),
                    Some("NN"),
                    Some("15"),
                ],
                vec![
                    Some("9"),
                    Some("weekend"),
                    Some("0"),
                    Some("10:00:00"),
                    None,
                    None,
                    Some("65"),
                    None,
                    None,
                    None,
                ],
            ],
        )
        .rows(
            ONE,
            &[
                "event_id",
                "event_name",
                "enabled",
                "location",
                "condition",
                "remarks",
                "action",
                "creator_name",
                "event_type",
            ],
            vec![vec![
                Some("4"),
                Some("my \"event\""),
                Some("Y"),
                Some("A"),
                None,
                Some("It's nightly"),
                Some("BEGIN\n  CALL purge();\nEND"),
                Some("DBA"),
                None,
            ]],
        )
        .rows(
            LIST,
            &[
                "event_id",
                "event_name",
                "enabled",
                "location",
                "condition",
                "remarks",
                "creator_name",
                "event_type",
            ],
            vec![
                vec![
                    Some("4"),
                    Some("my \"event\""),
                    Some("Y"),
                    Some("A"),
                    None,
                    Some("It's nightly"),
                    Some("DBA"),
                    None,
                ],
                vec![
                    Some("9"),
                    Some("twice"),
                    Some("N"),
                    Some("C"),
                    None,
                    None,
                    Some("DBA"),
                    None,
                ],
                vec![
                    Some("11"),
                    Some("low_disk"),
                    Some("Y"),
                    Some("A"),
                    Some("event_condition('DBFreePercent') < 10"),
                    None,
                    Some("DBA"),
                    Some("DBDiskSpace"),
                ],
                vec![
                    Some("12"),
                    Some("manual"),
                    Some("Y"),
                    Some("A"),
                    None,
                    None,
                    Some("DBA"),
                    None,
                ],
            ],
        )
}

#[test]
fn sqla_lists_events_with_their_schedules() {
    let mut f = sqla_fake();
    let l = list(preset("sqlanywhere"), &mut f, &ctx("")).unwrap();
    let by = |id: &str| l.jobs.iter().find(|j| j.id == id).unwrap();
    let e = by("4");
    assert_eq!(e.name, "my \"event\"");
    assert_eq!(e.kind, "Event");
    assert_eq!(
        e.schedule.as_deref(),
        Some("START TIME '03:00:00' ON ('Monday', 'Friday') START DATE '2026-01-01'")
    );
    assert!(
        e.enabled && e.can_run && e.can_enable && !e.has_history && e.has_script && !e.can_stop
    );
    assert_eq!(
        by("9").schedule.as_deref(),
        Some("BETWEEN '08:00:00' AND '18:00:00' EVERY 15 MINUTES ON (1, 0); START TIME '10:00:00' ON ('Sunday', 'Saturday')")
    );
    assert!(!by("9").enabled && !by("9").can_run);
    assert_eq!(
        by("11").schedule.as_deref(),
        Some("TYPE DBDiskSpace WHERE event_condition('DBFreePercent') < 10")
    );
    assert_eq!(by("12").schedule, None);
}

#[test]
fn sqla_controls_quote_the_name() {
    let mut f = sqla_fake();
    run(preset("sqlanywhere"), &mut f, &ctx(""), "4").unwrap();
    assert_eq!(f.log[0].1, vec!["4".to_string()], "the id is bound");
    assert_eq!(f.last().0, "TRIGGER EVENT \"my \"\"event\"\"\"");
    set_enabled(preset("sqlanywhere"), &mut f, "4", false).unwrap();
    assert_eq!(f.last().0, "ALTER EVENT \"my \"\"event\"\"\" DISABLE");
    let mut f = sqla_fake();
    for id in FORGED {
        forged(run(preset("sqlanywhere"), &mut f, &ctx(""), id).unwrap_err());
        forged(set_enabled(preset("sqlanywhere"), &mut f, id, true).unwrap_err());
        forged(script(preset("sqlanywhere"), &mut f, id).unwrap_err());
    }
    assert!(f.log.is_empty());
    assert!(matches!(
        runs(preset("sqlanywhere"), &mut f, "4", 5),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        stop(preset("sqlanywhere"), &mut f, "4"),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn sqla_detail_and_script() {
    let mut f = sqla_fake();
    let d = detail(preset("sqlanywhere"), &mut f, &ctx(""), "4").unwrap();
    assert_eq!(
        d.steps[0].command.as_deref(),
        Some("BEGIN\n  CALL purge();\nEND")
    );
    let sc = script(preset("sqlanywhere"), &mut f, "4").unwrap();
    assert_eq!(
        sc,
        "CREATE EVENT \"my \"\"event\"\"\"
SCHEDULE \"nightly\" START TIME '03:00:00' ON ('Monday', 'Friday') START DATE '2026-01-01'
HANDLER
BEGIN
  CALL purge();
END
go
COMMENT ON EVENT \"my \"\"event\"\"\" IS 'It''s nightly'
go
"
    );
}

// ---------------------------------------------------------------- IRIS

fn iris_fake() -> Fake {
    use super::iris::fixtures::*;
    Fake::default()
        .rows(
            NOW,
            &["LOCAL_NOW", "UTC_NOW"],
            vec![vec![Some(MINUS_3.0), Some(MINUS_3.1)]],
        )
        .rows(
            "%SYS_Task.History WHERE",
            &[
                "RUN_ID",
                "Name",
                "Status",
                "DisplayStatus",
                "Error",
                "LastStarted",
                "LastFinished",
                "DisplayStarted",
                "DisplayFinished",
                "Task",
            ],
            vec![
                vec![
                    Some("88"),
                    Some("Purge"),
                    Some("0"),
                    None,
                    None,
                    Some("67886,10800"),
                    None,
                    None,
                    None,
                    Some("3"),
                ],
                vec![
                    Some("87"),
                    Some("Purge"),
                    Some("1"),
                    Some("1"),
                    None,
                    Some("67885,10800"),
                    Some("67885,10805"),
                    Some("2026-11-11 03:00:00"),
                    Some("2026-11-11 03:00:05"),
                    Some("3"),
                ],
                vec![
                    Some("86"),
                    Some("Purge"),
                    Some("x"),
                    Some("ERROR #5002: boom"),
                    None,
                    None,
                    None,
                    Some("2026-11-10 03:00:00"),
                    Some("2026-11-10 03:00:01"),
                    Some("3"),
                ],
            ],
        )
        .rows("SELECT TOP 1 %ID", &["RUN_ID"], vec![vec![Some("1")]])
        .rows(
            LIST,
            &[
                "JOB_ID",
                "Name",
                "Description",
                "NameSpace",
                "TaskClass",
                "Suspended",
                "Error",
                "LastStarted",
                "LastFinished",
                "DayNextScheduled",
                "TimeNextScheduled",
                "TimePeriod",
                "TimePeriodEvery",
                "DailyFrequency",
                "DailyFrequencyTime",
                "DailyIncrement",
                "DailyStartTime",
                "DailyEndTime",
                "RunAsUser",
                "RunningJobNumber",
            ],
            vec![
                vec![
                    Some("3"),
                    Some("Purge"),
                    Some("Purges old data"),
                    Some("%SYS"),
                    Some("%SYS.Task.PurgeErrorsAndLogs"),
                    Some("0"),
                    None,
                    Some("67885,10800"),
                    Some("67885,10805"),
                    Some("2026-11-13"),
                    Some("03:00:00"),
                    Some("0"),
                    Some("1"),
                    Some("0"),
                    None,
                    None,
                    Some("10800"),
                    None,
                    Some("_SYSTEM"),
                    None,
                ],
                vec![
                    Some("7"),
                    Some("Often"),
                    None,
                    Some("USER"),
                    Some("User.Task"),
                    Some("1"),
                    Some("ERROR #5001"),
                    Some("2026-11-12 08:00:00"),
                    Some("2026-11-12 08:00:01"),
                    Some("67887"),
                    Some("28800"),
                    Some("1"),
                    Some("2"),
                    Some("1"),
                    Some("0"),
                    Some("15"),
                    Some("28800"),
                    Some("64800"),
                    None,
                    Some("1234"),
                ],
            ],
        )
}

#[test]
fn iris_reads_the_task_manager() {
    assert_eq!(super::iris::fixtures::horolog("47117,0"), Some(0));
    assert_eq!(
        super::iris::fixtures::horolog("67886,10800.5"),
        parse_ts("2026-11-12 03:00:00.500")
    );
    assert_eq!(
        super::iris::fixtures::horolog("67887"),
        parse_ts("2026-11-13")
    );
    assert_eq!(super::iris::fixtures::horolog("x,1"), None);
    let mut f = iris_fake();
    let l = list(preset("iris"), &mut f, &ctx("%SYS")).unwrap();
    assert!(l.notes.is_empty());
    let p = &l.jobs.iter().find(|j| j.id == "3").unwrap();
    assert_eq!(p.kind, "Task Manager");
    assert_eq!(p.schedule.as_deref(), Some("FREQ=DAILY;AT=03:00"));
    assert_eq!(p.next_run.as_deref(), Some("2026-11-13T06:00:00Z"));
    assert_eq!(p.last_outcome, Some(JobOutcome::Succeeded));
    assert_eq!(p.last_duration_ms, Some(5000));
    assert!(p.enabled && p.has_history && !p.can_run && !p.can_enable && !p.has_script);
    let o = &l.jobs.iter().find(|j| j.id == "7").unwrap();
    assert_eq!(
        o.schedule.as_deref(),
        Some("FREQ=WEEKLY;INTERVAL=2;EVERY=15MIN;BETWEEN=08:00-18:00")
    );
    assert!(!o.enabled && o.running && o.next_run.is_none());
    assert_eq!(o.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(o.last_started.as_deref(), Some("2026-11-12T11:00:00Z"));

    let mut f = iris_fake();
    let r = runs(preset("iris"), &mut f, "3", 50).unwrap();
    assert_eq!(f.last().1, vec!["3".to_string()]);
    assert!(f.last().0.starts_with("SELECT TOP 50 "));
    assert_eq!(
        r.iter().map(|r| r.outcome).collect::<Vec<_>>(),
        [
            JobOutcome::Running,
            JobOutcome::Succeeded,
            JobOutcome::Failed
        ]
    );
    assert_eq!(r[2].message.as_deref(), Some("ERROR #5002: boom"));
    assert_eq!(r[1].duration_ms, Some(5000));

    for e in [
        run(preset("iris"), &mut f, &ctx(""), "3").unwrap_err(),
        stop(preset("iris"), &mut f, "3").unwrap_err(),
        set_enabled(preset("iris"), &mut f, "3", true).unwrap_err(),
        script(preset("iris"), &mut f, "3").unwrap_err(),
    ] {
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }
    let mut f = iris_fake();
    for id in FORGED {
        forged(runs(preset("cache"), &mut f, id, 5).unwrap_err());
        forged(detail(preset("cache"), &mut f, &ctx(""), id).unwrap_err());
    }
    assert!(f.log.is_empty());
}

#[test]
fn iris_outside_sys_says_where_to_look() {
    let mut f = Fake::default().rows(super::iris::fixtures::LIST, &["JOB_ID"], vec![]);
    let l = list(preset("iris"), &mut f, &ctx("USER")).unwrap();
    assert!(l.jobs.is_empty() && l.notes[0].contains("%SYS"));
    let mut f = Fake::default().fail(super::iris::fixtures::LIST, Some("-99"));
    let l = list(preset("iris"), &mut f, &ctx("USER")).unwrap();
    assert!(l.notes[0].contains("%Admin_Task"));
}

#[test]
fn run_limits_are_capped() {
    let mut f = db2_fake();
    runs(preset("db2"), &mut f, "7", 100_000).unwrap();
    assert!(f.last().0.contains("FETCH FIRST 500 ROWS ONLY"));
    // No runs asked: none, without asking the server.
    let mut f = db2_fake();
    assert!(runs(preset("db2"), &mut f, "7", 0).unwrap().is_empty());
    assert!(f.log.is_empty());
    // Engines without history still say so.
    let mut f = db2i_fake();
    assert!(matches!(
        runs(preset("db2i"), &mut f, "000123/NIGHTLY", 0),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        runs(preset("teradata"), &mut f, "1", 0),
        Err(Error::Unsupported(_))
    ));
}

// ---------------------------------------------------------------- Hive

const HIVE_QUERY_COLS: &[&str] = &[
    "q.id",
    "q.name",
    "q.enabled",
    "q.namespace",
    "q.schedule",
    "q.owner",
    "q.query",
    "q.next_execution",
];

fn hive_fake(queries: Vec<Vec<Option<&str>>>) -> Fake {
    use super::hive::{NAMESPACE, NOW};
    Fake::default()
        // Three hours behind UTC: 12:00 UTC as seconds since the epoch.
        .rows(
            NOW,
            &["local_now", "utc_now"],
            vec![vec![Some("2026-11-12 09:00:00"), Some("1794484800")]],
        )
        .rows(
            NAMESPACE,
            &["set"],
            vec![vec![Some("hive.scheduled.queries.namespace=hive")]],
        )
        // The metastore's table: epoch seconds and the scheduled query's id.
        .rows(
            "FROM sys.scheduled_executions e",
            &[
                "id",
                "sq_id",
                "query_id",
                "state",
                "start_time",
                "end_time",
                "elapsed",
                "error_message",
            ],
            vec![
                // Query 2 (another namespace) is running too.
                vec![
                    Some("15"),
                    Some("2"),
                    Some("etl_20261112115820_dd"),
                    Some("EXECUTING"),
                    Some("1794484700"),
                    None,
                    None,
                    None,
                ],
                vec![
                    Some("14"),
                    Some("1"),
                    Some("dev_20261112120000_c9a3-1"),
                    Some("EXECUTING"),
                    Some("1794484800"),
                    None,
                    None,
                    None,
                ],
                vec![
                    Some("13"),
                    Some("1"),
                    Some("dev_20261112115000_aa"),
                    Some("FAILED"),
                    Some("1794484200"),
                    Some("1794484202"),
                    Some("2"),
                    Some("boom"),
                ],
                vec![
                    Some("12"),
                    Some("1"),
                    Some("dev_20261112114000_bb"),
                    Some("FINISHED"),
                    Some("1794483600"),
                    Some("1794483601"),
                    Some("1"),
                    None,
                ],
            ],
        )
        // The view (by name), for users who can't read sys.
        .rows(
            "FROM information_schema.scheduled_executions e",
            &[
                "id",
                "name",
                "query_id",
                "state",
                "start_time",
                "end_time",
                "elapsed",
                "error_message",
            ],
            vec![
                vec![
                    Some("14"),
                    Some("sc1"),
                    Some("dev_20261112120000_c9a3-1"),
                    Some("EXECUTING"),
                    Some("2026-11-12 09:00:00"),
                    None,
                    None,
                    None,
                ],
                vec![
                    Some("13"),
                    Some("sc1"),
                    Some("dev_20261112115000_aa"),
                    Some("FAILED"),
                    Some("2026-11-12 08:50:00"),
                    Some("2026-11-12 08:50:02"),
                    Some("2"),
                    Some("boom"),
                ],
                vec![
                    Some("12"),
                    Some("sc1"),
                    Some("dev_20261112114000_bb"),
                    Some("FINISHED"),
                    Some("2026-11-12 08:40:00"),
                    Some("2026-11-12 08:40:01"),
                    Some("1"),
                    None,
                ],
            ],
        )
        .rows(
            "FROM information_schema.scheduled_queries q",
            HIVE_QUERY_COLS,
            queries,
        )
}

fn sc1() -> Vec<Option<&'static str>> {
    vec![
        Some("1"),
        Some("sc1"),
        Some("true"),
        Some("hive"),
        Some("0 */10 * * * ? *"),
        Some("dev"),
        Some("insert into t values (1)"),
        Some("2026-11-12 10:00:00"),
    ]
}

fn elsewhere() -> Vec<Option<&'static str>> {
    vec![
        Some("2"),
        Some("other"),
        Some("false"),
        Some("warehouse"),
        Some("0 0 3 * * ? *"),
        Some("etl"),
        Some("select 1"),
        None,
    ]
}

#[test]
fn hive_lists_scheduled_queries() {
    let mut f = hive_fake(vec![sc1(), elsewhere()]);
    let l = list(preset("hive"), &mut f, &ctx("default")).unwrap();
    assert_eq!(
        l.notes,
        ["Las consultas programadas de otros namespaces se ven solo para lectura."]
    );
    let j = l.jobs.iter().find(|j| j.id == "1").unwrap();
    assert_eq!(
        (j.name.as_str(), j.kind.as_str()),
        ("sc1", "Scheduled query")
    );
    assert_eq!(j.schedule.as_deref(), Some("0 */10 * * * ? *"));
    assert_eq!(j.next_run.as_deref(), Some("2026-11-12T13:00:00Z"));
    assert!(j.enabled && j.can_run && j.can_enable && j.has_history && j.has_script);
    assert!(j.running && j.can_stop);
    assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(j.last_started.as_deref(), Some("2026-11-12T11:50:00Z"));
    assert_eq!(j.last_duration_ms, Some(2000));
    // Another namespace: read only, also while it runs (stop goes through
    // the same namespace check as ALTER).
    let o = l.jobs.iter().find(|j| j.id == "2").unwrap();
    assert!(!o.enabled && !o.can_run && !o.can_enable && o.has_script);
    assert!(o.running && !o.can_stop);
    assert_eq!(o.group.as_deref(), Some("warehouse"));
    assert!(f.sent("FROM sys.scheduled_executions e"));
    assert!(!f.sent("FROM information_schema.scheduled_executions e"));
    // The detail says the same.
    let mut f = hive_fake(vec![elsewhere()]);
    let d = detail(preset("hive"), &mut f, &ctx(""), "2").unwrap();
    assert!(d.job.running && !d.job.can_stop);
    assert!(matches!(
        stop(preset("hive"), &mut f, "2"),
        Err(Error::Unsupported(_))
    ));
    assert!(!f.sent("KILL"));

    let mut f = Fake::default().fail("FROM information_schema.scheduled_queries q", Some("10001"));
    let l = list(preset("cloudera"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.is_empty() && l.notes[0].contains("Hive 4.0"));
}

#[test]
fn hive_without_sys_matches_executions_by_unique_names() {
    // sys can't be read: the view, by name.
    let mut f = hive_fake(vec![sc1(), elsewhere()]).fail("FROM sys.scheduled_executions e", None);
    f.replies.rotate_right(1);
    let l = list(preset("hive"), &mut f, &ctx("")).unwrap();
    let j = l.jobs.iter().find(|j| j.id == "1").unwrap();
    assert!(j.running && j.can_stop);
    assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(j.last_started.as_deref(), Some("2026-11-12T11:50:00Z"));
    let r = runs(preset("hive"), &mut f, "1", 10).unwrap();
    assert_eq!(r.len(), 3);
    assert!(f.last().0.contains("WHERE e.schedule_name = 'sc1'"));
    stop(preset("hive"), &mut f, "1").unwrap();
    assert_eq!(f.last().0, "KILL QUERY 'dev_20261112120000_c9a3-1'");

    // A name that another namespace uses too: the view can't tell their
    // executions apart, so none are shown and none are stopped.
    let mut twin = elsewhere();
    twin[1] = Some("sc1");
    let mut f = hive_fake(vec![sc1(), twin]).fail("FROM sys.scheduled_executions e", None);
    f.replies.rotate_right(1);
    let l = list(preset("hive"), &mut f, &ctx("")).unwrap();
    assert!(l
        .jobs
        .iter()
        .all(|j| !j.running && j.last_outcome.is_none()));
    let e = runs(preset("hive"), &mut f, "1", 10).unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.contains("se mezclan")),
        "{e:?}"
    );
    assert!(stop(preset("hive"), &mut f, "1").is_err());
    assert!(!f.sent("KILL"));
    assert!(!f.sent("FROM information_schema.scheduled_executions e\n WHERE"));
}

#[test]
fn hive_controls_runs_and_script() {
    let mut f = hive_fake(vec![sc1()]);
    run(preset("hive"), &mut f, &ctx(""), "1").unwrap();
    assert_eq!(f.last().0, "ALTER SCHEDULED QUERY `sc1` EXECUTE");
    set_enabled(preset("hive"), &mut f, "1", false).unwrap();
    assert_eq!(f.last().0, "ALTER SCHEDULED QUERY `sc1` DISABLED");
    set_enabled(preset("hive"), &mut f, "1", true).unwrap();
    assert_eq!(f.last().0, "ALTER SCHEDULED QUERY `sc1` ENABLED");
    stop(preset("hive"), &mut f, "1").unwrap();
    assert_eq!(f.last().0, "KILL QUERY 'dev_20261112120000_c9a3-1'");
    assert!(f.sent("WHERE q.scheduled_query_id = 1"));
    // Only query 1's execution: not the one of query 2 running elsewhere.
    assert!(!f.sent("KILL QUERY 'etl_"));

    let mut f = hive_fake(vec![sc1()]);
    let r = runs(preset("hive"), &mut f, "1", 10).unwrap();
    let (sql, _) = f.last();
    assert!(
        sql.contains("FROM sys.scheduled_executions e\n WHERE e.scheduled_query_id = 1")
            && sql.contains("LIMIT 10"),
        "{sql}"
    );
    assert_eq!(r[0].started.as_deref(), Some("2026-11-12T12:00:00Z"));
    assert_eq!(
        r.iter().map(|r| r.outcome).collect::<Vec<_>>(),
        [
            JobOutcome::Running,
            JobOutcome::Failed,
            JobOutcome::Succeeded
        ]
    );
    assert_eq!(r[1].message.as_deref(), Some("boom"));
    assert_eq!(r[1].duration_ms, Some(2000));

    let mut f = hive_fake(vec![sc1()]);
    assert_eq!(
        script(preset("hive"), &mut f, "1").unwrap(),
        "CREATE SCHEDULED QUERY `sc1`\nCRON '0 */10 * * * ? *'\nEXECUTED AS `dev`\nENABLED\nDEFINED AS insert into t values (1);\n"
    );
    let d = detail(preset("hive"), &mut f, &ctx(""), "1").unwrap();
    assert_eq!(
        d.steps[0].command.as_deref(),
        Some("insert into t values (1)")
    );

    // Another namespace, or a name that isn't a plain identifier: no ALTER.
    let mut f = hive_fake(vec![elsewhere()]);
    assert!(matches!(
        run(preset("hive"), &mut f, &ctx(""), "2"),
        Err(Error::Unsupported(_))
    ));
    let mut odd = sc1();
    odd[1] = Some("we`ird'; DROP");
    let mut f = hive_fake(vec![odd]);
    assert!(matches!(
        set_enabled(preset("hive"), &mut f, "1", true),
        Err(Error::Unsupported(_))
    ));
    assert!(!f.sent("ALTER"));
    let l = list(preset("hive"), &mut f, &ctx("")).unwrap();
    assert!(!l.jobs[0].can_enable && !l.jobs[0].has_script);

    let mut f = hive_fake(vec![sc1()]);
    for id in FORGED {
        forged(run(preset("hive"), &mut f, &ctx(""), id).unwrap_err());
        forged(stop(preset("hive"), &mut f, id).unwrap_err());
        forged(runs(preset("hive"), &mut f, id, 5).unwrap_err());
        forged(script(preset("hive"), &mut f, id).unwrap_err());
    }
    assert!(f.log.is_empty());
}

// The engines added after the first pass, one file each.
mod altibase;
mod dameng;
mod vertica;
mod virtuoso;

/// A SQL text built at run time, as a [`Fake`] key.
fn key(sql: String) -> &'static str {
    Box::leak(sql.into_boxed_str())
}

/// `id` never reached the SQL text: it went, if at all, as a parameter.
fn only_bound(f: &Fake, id: &str) {
    if id.trim().is_empty() {
        return;
    }
    for (sql, _) in &f.log {
        assert!(!sql.contains(id), "{id:?} in {sql}");
    }
}

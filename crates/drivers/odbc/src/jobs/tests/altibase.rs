//! Altibase's jobs.

use super::*;
use crate::jobs::altibase::fixtures::*;

const COLS: &[&str] = &[
    "JOB_ID",
    "JOB_NAME",
    "EXEC_QUERY",
    "START_TIME",
    "END_TIME",
    "INTERVAL",
    "INTERVAL_TYPE",
    "STATE",
    "LAST_EXEC_TIME",
    "EXEC_COUNT",
    "ERROR_CODE",
    "IS_ENABLE",
    "COMMENT",
];

fn job1() -> Vec<Option<&'static str>> {
    vec![
        Some("3"),
        Some("JOB1"),
        Some("SYS.PROC1(1, 'x')"),
        Some("2026-08-01 00:00:00"),
        Some("2026-12-31 00:00:00"),
        Some("1"),
        Some("DD"),
        Some("0"),
        Some("2026-11-12 08:00:00"),
        Some("43"),
        None,
        Some("T"),
        Some("It's daily"),
    ]
}

fn rows() -> Vec<Vec<Option<&'static str>>> {
    vec![
        job1(),
        vec![
            Some("4"),
            Some("JOB2"),
            Some("EXEC PROC2"),
            Some("2026-11-01 10:00:00"),
            None,
            Some("10"),
            Some("MI"),
            Some("1"),
            Some("2026-11-12 08:55:00"),
            Some("7"),
            Some("0201001"),
            Some("F"),
            None,
        ],
        vec![
            Some("5"),
            Some("ONCE"),
            Some("PROC3"),
            Some("2026-12-01 00:00:00"),
            None,
            Some("0"),
            None,
            Some("0"),
            None,
            Some("0"),
            None,
            Some("T"),
            None,
        ],
    ]
}

fn fake(user: &'static str, scheduler: &'static str) -> Fake {
    Fake::default()
        .rows(
            NOW,
            &["LOCAL_NOW", "UTC_NOW"],
            vec![vec![
                Some("2026-11-12 09:00:00"),
                Some("2026-11-12 12:00:00"),
            ]],
        )
        .rows(USER, &["U"], vec![vec![Some(user)]])
        .rows(
            PROPS,
            &["NAME", "VALUE1"],
            vec![
                vec![Some("JOB_SCHEDULER_ENABLE"), Some(scheduler)],
                vec![Some("JOB_THREAD_COUNT"), Some("2")],
            ],
        )
        .rows(key(select(true, true)), COLS, vec![job1()])
        .rows(key(select(true, false)), COLS, rows())
}

#[test]
fn altibase_lists_jobs() {
    let mut f = fake("SYS", "1");
    let l = list(preset("altibase"), &mut f, &ctx("")).unwrap();
    assert!(l.notes.is_empty(), "{:?}", l.notes);
    let by = |id: &str| l.jobs.iter().find(|j| j.id == id).unwrap();
    let j1 = by("3");
    assert_eq!(j1.name, "JOB1");
    assert_eq!(j1.kind, "Job");
    assert_eq!(
        j1.schedule.as_deref(),
        Some("START 2026-08-01 00:00:00 END 2026-12-31 00:00:00 INTERVAL 1 DAY")
    );
    assert_eq!(j1.description.as_deref(), Some("It's daily"));
    assert!(j1.enabled && j1.can_enable && j1.has_script && !j1.running);
    assert!(!j1.can_run && !j1.can_stop && !j1.has_history);
    assert_eq!(j1.last_outcome, Some(JobOutcome::Succeeded));
    assert_eq!(j1.last_started.as_deref(), Some("2026-11-12T11:00:00Z"));
    let j2 = by("4");
    assert!(j2.running && !j2.enabled);
    assert_eq!(j2.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(
        j2.schedule.as_deref(),
        Some("START 2026-11-01 10:00:00 INTERVAL 10 MINUTE")
    );
    let once = by("5");
    assert_eq!(once.schedule.as_deref(), Some("START 2026-12-01 00:00:00"));
    assert_eq!(once.last_outcome, None);
}

#[test]
fn altibase_notes_and_users_other_than_sys() {
    let mut f = fake("APP", "0");
    let l = list(preset("altibase"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.iter().all(|j| !j.can_enable));
    assert!(l.notes[0].contains("JOB_SCHEDULER_ENABLE"), "{:?}", l.notes);
    let e = set_enabled(preset("altibase"), &mut f, "3", false).unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    assert!(!f.sent("ALTER JOB"));

    // Before CREATE JOB: no SYS_JOBS_.
    let mut f = Fake::default()
        .fail(key(select(true, false)), None)
        .fail(key(select(false, false)), None)
        .rows(EXISTS, &["N"], vec![vec![Some("0")]]);
    let l = list(preset("altibase"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.is_empty());
    assert!(l.notes[0].contains("6.3.1"), "{:?}", l.notes);
    // The table is there: the failure is real.
    let mut f = Fake::default()
        .fail(key(select(true, false)), None)
        .fail(key(select(false, false)), None)
        .rows(EXISTS, &["N"], vec![vec![Some("1")]]);
    assert!(list(preset("altibase"), &mut f, &ctx("")).is_err());
}

#[test]
fn altibase_detail_enable_and_script() {
    let mut f = fake("sys", "1");
    let d = detail(preset("altibase"), &mut f, &ctx(""), "3").unwrap();
    assert_eq!(d.steps[0].command.as_deref(), Some("SYS.PROC1(1, 'x')"));
    let label = |l: &str| {
        d.properties
            .iter()
            .find(|p| p.label == l)
            .map(|p| p.value.as_str())
    };
    assert_eq!(label("Intervalo"), Some("1 DAY"));
    assert_eq!(label("Ejecuciones"), Some("43"));
    assert_eq!(f.log[0].1, vec!["3".to_string()], "the id is bound");

    set_enabled(preset("altibase"), &mut f, "3", false).unwrap();
    assert_eq!(f.last().0, "ALTER JOB \"JOB1\" SET DISABLE");
    set_enabled(preset("altibase"), &mut f, " 3 ", true).unwrap();
    assert_eq!(f.last().0, "ALTER JOB \"JOB1\" SET ENABLE");

    let sc = script(preset("altibase"), &mut f, "3").unwrap();
    assert_eq!(
        sc,
        "CREATE JOB \"JOB1\"
EXEC SYS.PROC1(1, 'x')
START TO_DATE('2026-08-01 00:00:00', 'YYYY-MM-DD HH24:MI:SS')
END TO_DATE('2026-12-31 00:00:00', 'YYYY-MM-DD HH24:MI:SS')
INTERVAL 1 DAY
ENABLE
COMMENT 'It''s daily';
"
    );
    for e in [
        run(preset("altibase"), &mut f, &ctx(""), "3").unwrap_err(),
        stop(preset("altibase"), &mut f, "3").unwrap_err(),
        runs(preset("altibase"), &mut f, "3", 5).unwrap_err(),
    ] {
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }
}

#[test]
fn altibase_before_enable_and_forged_ids() {
    // 6.3: no IS_ENABLE nor COMMENT.
    let old = |r: Vec<Option<&'static str>>| r[..11].to_vec();
    let mut f = Fake::default()
        .rows(USER, &["U"], vec![vec![Some("SYS")]])
        .fail(key(select(true, true)), None)
        .fail(key(select(true, false)), None)
        .rows(key(select(false, true)), &COLS[..11], vec![old(job1())])
        .rows(
            key(select(false, false)),
            &COLS[..11],
            rows().into_iter().map(old).collect(),
        );
    let l = list(preset("altibase"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.iter().all(|j| j.enabled && !j.can_enable));
    assert!(matches!(
        set_enabled(preset("altibase"), &mut f, "3", true),
        Err(Error::Unsupported(_))
    ));
    let sc = script(preset("altibase"), &mut f, "3").unwrap();
    assert!(sc.ends_with("INTERVAL 1 DAY;\n"), "{sc}");

    let mut f = fake("SYS", "1");
    for id in FORGED {
        forged(detail(preset("altibase"), &mut f, &ctx(""), id).unwrap_err());
        forged(set_enabled(preset("altibase"), &mut f, id, true).unwrap_err());
        forged(script(preset("altibase"), &mut f, id).unwrap_err());
    }
    assert!(f.log.is_empty());
    // An id nobody has.
    let mut f = Fake::default().rows(key(select(true, true)), COLS, Vec::new());
    let e = script(preset("altibase"), &mut f, "99").unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.contains("no existe")),
        "{e:?}"
    );
}

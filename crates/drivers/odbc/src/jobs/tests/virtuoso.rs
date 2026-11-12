//! Virtuoso's scheduled events.

use super::*;
use crate::jobs::virtuoso::fixtures::*;

const COLS: &[&str] = &[
    "SE_NAME",
    "SE_START",
    "SE_INTERVAL",
    "SE_LAST_COMPLETED",
    "SE_SQL",
    "SE_LAST_ERROR",
    "SE_ENABLE_NOTIFY",
    "SE_NOTIFY",
    "SE_DISABLED",
];

const OLD_COLS: &[&str] = &[
    "SE_NAME",
    "SE_START",
    "SE_INTERVAL",
    "SE_LAST_COMPLETED",
    "SE_SQL",
];

fn nightly() -> Vec<Option<&'static str>> {
    vec![
        Some("nightly 'purge'"),
        Some("2026-01-01 03:00:00.000000"),
        Some("60"),
        // 08:30 local: due again at 09:30, half an hour from now.
        Some("2026-11-12 08:30:00.000000"),
        Some("DB.DBA.PURGE ('a\\b')"),
        None,
        Some("1"),
        Some("dba@example.com"),
        Some("0"),
    ]
}

fn rows() -> Vec<Vec<Option<&'static str>>> {
    vec![
        nightly(),
        vec![
            Some("broken"),
            Some("2026-11-01 00:00:00"),
            Some("1"),
            Some("2026-11-12 08:59:00"),
            Some("select 1/0"),
            Some("SR012: Division by zero"),
            Some("0"),
            None,
            Some("1"),
        ],
        vec![
            Some("never"),
            Some("2026-11-01 00:00:00"),
            Some("0"),
            None,
            Some("select 1"),
            None,
            Some("0"),
            None,
            Some("0"),
        ],
    ]
}

fn fake(scheduler: &'static str) -> Fake {
    Fake::default()
        .rows(
            NOW,
            &["LOCAL_NOW", "UTC_NOW"],
            vec![vec![Some(MINUS_3.0), Some("2026-11-12 12:00:00.000000Z")]],
        )
        .rows(INTERVAL, &["SI"], vec![vec![Some(scheduler)]])
        .on(UPDATE, Reply::Affected(1))
        .rows(key(select(true, true)), COLS, vec![nightly()])
        .rows(key(select(true, false)), COLS, rows())
}

/// A server from before SE_DISABLED: the full select fails.
fn old_fake() -> Fake {
    let old = |r: Vec<Option<&'static str>>| r[..5].to_vec();
    Fake::default()
        .fail(key(select(true, true)), None)
        .fail(key(select(true, false)), None)
        .rows(key(select(false, true)), OLD_COLS, vec![old(nightly())])
        .rows(
            key(select(false, false)),
            OLD_COLS,
            rows().into_iter().map(old).collect(),
        )
}

#[test]
fn virtuoso_lists_scheduled_events() {
    let mut f = fake("10");
    let l = list(preset("virtuoso"), &mut f, &ctx("")).unwrap();
    assert!(l.notes.is_empty(), "{:?}", l.notes);
    let by = |id: &str| l.jobs.iter().find(|j| j.id == id).unwrap();
    let n = by("nightly 'purge'");
    assert_eq!(n.name, "nightly 'purge'");
    assert_eq!(n.kind, "Scheduler");
    assert_eq!(
        n.schedule.as_deref(),
        Some("Cada 60 minutos, desde 2026-01-01 03:00:00")
    );
    assert!(n.enabled && n.can_enable && n.has_script);
    assert!(!n.can_run && !n.can_stop && !n.has_history && !n.running);
    assert_eq!(n.last_outcome, Some(JobOutcome::Succeeded));
    assert_eq!(n.last_started.as_deref(), Some("2026-11-12T11:30:00Z"));
    assert_eq!(n.next_run.as_deref(), Some("2026-11-12T12:30:00Z"));
    let b = by("broken");
    assert!(!b.enabled);
    assert_eq!(
        b.schedule.as_deref(),
        Some("Cada minuto, desde 2026-11-01 00:00:00")
    );
    assert_eq!(b.last_outcome, Some(JobOutcome::Failed));
    assert_eq!(b.next_run, None, "disabled");
    let never = by("never");
    assert_eq!(never.schedule, None, "no interval: the scheduler skips it");
    assert_eq!(never.next_run, None);
    assert_eq!(never.last_outcome, None);
}

#[test]
fn virtuoso_says_when_the_scheduler_is_off_or_hidden() {
    let mut f = fake("0");
    let l = list(preset("virtuoso"), &mut f, &ctx("")).unwrap();
    assert_eq!(l.jobs.len(), 3);
    assert!(
        l.notes[0].contains("SchedulerInterval = 0"),
        "{:?}",
        l.notes
    );

    let mut f = Fake::default().on(
        key(select(true, false)),
        Reply::Fail(
            None,
            "SQ033: SELECT access denied for column SE_NAME of table DB.DBA.SYS_SCHEDULED_EVENT",
        ),
    );
    let l = list(preset("virtuoso"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.is_empty());
    assert!(l.notes[0].contains("dba"), "{:?}", l.notes);
    // Not the older columns' fallback: the user can't read the table.
    assert_eq!(f.log.len(), 1);

    // Anything else is an error.
    let mut f = Fake::default()
        .fail(key(select(true, false)), None)
        .fail(key(select(false, false)), None);
    assert!(list(preset("virtuoso"), &mut f, &ctx("")).is_err());
}

#[test]
fn virtuoso_detail_enable_and_script() {
    let mut f = fake("10");
    let d = detail(preset("virtuoso"), &mut f, &ctx(""), "nightly 'purge'").unwrap();
    assert_eq!(d.job.id, "nightly 'purge'");
    assert_eq!(d.steps.len(), 1);
    assert_eq!(d.steps[0].command.as_deref(), Some("DB.DBA.PURGE ('a\\b')"));
    let label = |l: &str| {
        d.properties
            .iter()
            .find(|p| p.label == l)
            .map(|p| p.value.as_str())
    };
    assert_eq!(label("Intervalo (minutos)"), Some("60"));
    assert_eq!(label("Aviso por correo a"), Some("dba@example.com"));
    // The name goes only as a parameter.
    assert_eq!(f.log[0].1, vec!["nightly 'purge'".to_string()]);
    only_bound(&f, "nightly 'purge'");

    set_enabled(preset("virtuoso"), &mut f, "nightly 'purge'", false).unwrap();
    assert_eq!(
        f.last().0,
        "UPDATE DB.DBA.SYS_SCHEDULED_EVENT SET SE_DISABLED = 1 WHERE SE_NAME = ?"
    );
    assert_eq!(f.last().1, vec!["nightly 'purge'".to_string()]);
    set_enabled(preset("virtuoso"), &mut f, "nightly 'purge'", true).unwrap();
    assert!(f.last().0.contains("SE_DISABLED = 0 WHERE"));

    let sc = script(preset("virtuoso"), &mut f, "nightly 'purge'").unwrap();
    assert_eq!(
        sc,
        "INSERT INTO DB.DBA.SYS_SCHEDULED_EVENT (SE_NAME, SE_START, SE_INTERVAL, SE_SQL, SE_DISABLED, SE_ENABLE_NOTIFY, SE_NOTIFY)
VALUES ('nightly ''purge''', CAST('2026-01-01 03:00:00' AS DATETIME), 60, 'DB.DBA.PURGE (''a\\\\b'')', 0, 1, 'dba@example.com');
"
    );

    // Nothing to run, stop or read back.
    for e in [
        run(preset("virtuoso"), &mut f, &ctx(""), "nightly 'purge'").unwrap_err(),
        stop(preset("virtuoso"), &mut f, "nightly 'purge'").unwrap_err(),
        runs(preset("virtuoso"), &mut f, "nightly 'purge'", 5).unwrap_err(),
    ] {
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }
}

#[test]
fn virtuoso_unknown_and_forged_names() {
    // No row for it: not found, and nothing changed.
    let mut f = Fake::default().rows(key(select(true, true)), COLS, Vec::new());
    let e = set_enabled(preset("virtuoso"), &mut f, "x'; DROP TABLE t; --", false).unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.contains("no existe")),
        "{e:?}"
    );
    assert!(!f.sent("UPDATE"));
    only_bound(&f, "x'; DROP TABLE t; --");

    let mut f = fake("10");
    for id in ["", "  ", "a\u{0}b", "a\nb"] {
        forged(detail(preset("virtuoso"), &mut f, &ctx(""), id).unwrap_err());
        forged(set_enabled(preset("virtuoso"), &mut f, id, true).unwrap_err());
        forged(script(preset("virtuoso"), &mut f, id).unwrap_err());
    }
    forged(script(preset("virtuoso"), &mut f, &"x".repeat(1001)).unwrap_err());
    assert!(f.log.is_empty());
    // Whatever else comes is only a name to look up, bound.
    for id in FORGED
        .iter()
        .filter(|i| !i.trim().is_empty() && !i.contains('\0'))
    {
        let mut f = fake("10");
        script(preset("virtuoso"), &mut f, id).unwrap();
        assert_eq!(f.log[0].1, vec![id.trim().to_string()]);
        if id.len() > 2 {
            only_bound(&f, id);
        }
    }
}

#[test]
fn virtuoso_without_se_disabled() {
    let mut f = old_fake();
    let l = list(preset("virtuoso"), &mut f, &ctx("")).unwrap();
    assert_eq!(l.jobs.len(), 3);
    assert!(l.jobs.iter().all(|j| j.enabled && !j.can_enable));
    assert_eq!(l.jobs[0].last_outcome, Some(JobOutcome::Succeeded));
    let e = set_enabled(preset("virtuoso"), &mut f, "nightly 'purge'", false).unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    assert!(!f.sent("UPDATE"));
    let sc = script(preset("virtuoso"), &mut f, "nightly 'purge'").unwrap();
    assert!(sc.starts_with(
        "INSERT INTO DB.DBA.SYS_SCHEDULED_EVENT (SE_NAME, SE_START, SE_INTERVAL, SE_SQL)\n"
    ));
}

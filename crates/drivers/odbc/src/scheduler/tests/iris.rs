//! InterSystems IRIS and Caché: the Task Manager, read only.

use super::*;

const COLS: &[&str] = &[
    "JOB_ID",
    "Name",
    "Description",
    "TaskClass",
    "NameSpace",
    "RunAsUser",
    "Suspended",
    "TimePeriod",
    "DailyFrequency",
    "DailyStartTime",
    "LastStarted",
    "DayNextScheduled",
    "Settings",
    "MirrorStatus",
];

fn server() -> Fake {
    let row = vec![
        Some("12"),
        Some("Purge Journal"),
        Some("Borra los journals viejos"),
        Some("%SYS.Task.PurgeJournal"),
        Some("%SYS"),
        Some("_SYSTEM"),
        Some("0"),
        Some("0"),
        Some("0"),
        Some("7200"),
        Some("2026-11-11 02:00:01"),
        Some("67521"),
        None,
        Some("3"),
    ];
    Fake::default()
        .rows("WHERE %ID = ?", COLS, vec![row.clone()])
        .rows("FROM %SYS.Task", COLS, vec![row])
}

#[test]
fn the_task_manager_is_shown_whole_and_read_only() {
    let i = info(preset("iris"), &mut server(), &ctx()).unwrap();
    assert_eq!(i.kinds[0].key, "job");
    assert!(!i.kinds[0].can_create && !i.kinds[0].can_edit && !i.kinds[0].can_delete);
    assert!(i.notes[0].contains("ObjectScript"));
    let f = form(preset("cache"), &mut server(), &ctx(), "job", Some("12")).unwrap();
    well_formed(&f);
    assert!(f.read_only);
    assert!(f
        .sections
        .iter()
        .flat_map(|s| &s.fields)
        .all(|x| x.read_only));
    assert_eq!(val(&f, "Name"), "Purge Journal");
    assert_eq!(val(&f, "TaskClass"), "%SYS.Task.PurgeJournal");
    assert_eq!(val(&f, "Suspended"), false);
    assert_eq!(val(&f, "TimePeriod"), "0");
    assert_eq!(val(&f, "DailyStartTime"), "02:00:00");
    assert_eq!(val(&f, "LastStarted"), "2026-11-11T02:00:01");
    // $HOROLOG day 67521 is 2025-11-12.
    assert_eq!(val(&f, "DayNextScheduled"), "2025-11-12T00:00:00");
    assert_eq!(val(&f, "id"), "12");
    assert_eq!(val(&f, "other"), "MirrorStatus: 3");
    let l = items(preset("iris"), &mut server(), &ctx(), "job").unwrap();
    assert_eq!(l.items[0].id, "12");
    assert_eq!(l.items[0].enabled, Some(true));
}

#[test]
fn nothing_is_written_and_why_is_said() {
    let v = fv(json!({"Name": "x"}), json!({}));
    for r in [
        script(preset("iris"), &mut server(), "job", None, &v),
        script(preset("iris"), &mut server(), "job", Some("12"), &v),
        delete_script(preset("iris"), &mut server(), "job", "12"),
    ] {
        assert!(matches!(r, Err(Error::Unsupported(m)) if m.contains("%SYS.Task")));
    }
    assert!(matches!(
        form(preset("iris"), &mut server(), &ctx(), "job", None),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        clear_history(preset("iris"), &mut server(), "12"),
        Err(Error::Unsupported(m)) if m.contains("purga")
    ));
    assert!(matches!(
        delete(preset("iris"), &mut server(), "job", "12"),
        Err(Error::Unsupported(_))
    ));
    let mut f = Fake::default().fail("FROM %SYS.Task", Some("-99"));
    let i = info(preset("iris"), &mut f, &ctx()).unwrap();
    assert!(i.kinds.is_empty() && i.notes[0].contains("%Admin_Task"));
    assert!(refused(form(
        preset("iris"),
        &mut server(),
        &ctx(),
        "job",
        Some("x")
    ))
    .contains("id de job"));
}

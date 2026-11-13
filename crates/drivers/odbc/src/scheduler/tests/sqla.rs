//! SQL Anywhere: events.

use super::*;
use crate::scheduler::sqla as e;

const EV_COLS: &[&str] = &[
    "event_id",
    "event_name",
    "enabled",
    "location",
    "condition",
    "remarks",
    "action",
    "creator_name",
    "event_type",
];
const SCH_COLS: &[&str] = &[
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
];

/// Event 5 `ev_nightly` (two schedules), event 6 `ev_space` (a system
/// event with two conditions).
fn server() -> Fake {
    Fake::default()
        .rows_with(
            "WHERE E.event_id = ?",
            "5",
            EV_COLS,
            vec![vec![
                Some("5"),
                Some("ev_nightly"),
                Some("Y"),
                None,
                None,
                Some("it's nightly"),
                Some("BEGIN\n  CALL sp_cleanup();\nEND"),
                Some("DBA"),
                None,
            ]],
        )
        .rows_with(
            "WHERE E.event_id = ?",
            "6",
            EV_COLS,
            vec![vec![
                Some("6"),
                Some("ev_space"),
                Some("N"),
                None,
                Some("event_condition( 'DBFreePercent' ) < 20 AND event_condition('Interval') >= 600"),
                None,
                Some("BEGIN MESSAGE 'low' TO CONSOLE; END"),
                Some("DBA"),
                Some("DBDiskSpace"),
            ]],
        )
        .rows(
            "FROM SYS.SYSSCHEDULE",
            SCH_COLS,
            vec![
                vec![
                    Some("5"),
                    Some("nightly"),
                    Some("N"),
                    Some("03:00:00"),
                    None,
                    Some("2026-11-12"),
                    Some("0"),
                    Some("0"),
                    None,
                    None,
                ],
                vec![
                    Some("5"),
                    Some("office"),
                    Some("Y"),
                    Some("08:00:00"),
                    Some("18:00:00"),
                    None,
                    Some("34"),
                    Some("0"),
                    Some("NN"),
                    Some("30"),
                ],
            ],
        )
        .rows(
            "SELECT name FROM SYS.SYSEVENTTYPE",
            &["name"],
            vec![
                vec![Some("DBDiskSpace")],
                vec![Some("GrowDB")],
                vec![Some("RAISERROR")],
            ],
        )
}

fn form_of(id: &str) -> ObjectForm {
    form(
        preset("sqlanywhere"),
        &mut server(),
        &ctx(),
        "job",
        Some(id),
    )
    .unwrap()
}

#[test]
fn an_event_is_triggered_now_and_its_lists_follow_what_fires_it() {
    let p = preset("sqlanywhere");
    let i = info(p, &mut server(), &ctx()).unwrap();
    let keys: Vec<&str> = i.kinds[0].actions.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, ["run"]);
    // Events keep no history.
    assert!(!i.kinds[0].clear_history && !i.kinds[0].run_from_step);
    let s = action_script(p, &mut server(), &ctx(), "job", "5", "run").unwrap();
    assert!(
        s.starts_with("-- Dispara el evento ahora (TRIGGER EVENT)")
            && s.contains("TRIGGER EVENT ")
            && s.contains("ev_nightly")
            && s.ends_with("\ngo\n"),
        "{s}"
    );
    let mut f = server();
    action(p, &mut f, &ctx(), "job", "5", "run").unwrap();
    assert!(f.sent("TRIGGER EVENT"));
    assert!(matches!(
        action_script(p, &mut server(), &ctx(), "job", "5", "stop"),
        Err(Error::Unsupported(_))
    ));
    assert!(action_script(p, &mut server(), &ctx(), "job", "99", "run").is_err());
    // The conditions only with a system event; the schedules (at least
    // one) only when they fire it.
    let f = form_of("5");
    let cond = f
        .sections
        .iter()
        .flat_map(|s| &s.lists)
        .find(|l| l.key == "conditions")
        .unwrap();
    assert_eq!(cond.when, Some(("mode".into(), vec!["type".into()])));
    let sched = f.sections.iter().find(|s| s.key == "schedules").unwrap();
    let on_schedule = Some(("mode".into(), vec!["schedule".into()]));
    assert_eq!(sched.when, on_schedule);
    assert_eq!(sched.lists[0].when, on_schedule);
    assert_eq!(sched.lists[0].min_rows, 1);
    // A manual event sends neither list: nothing is missing.
    let mut v = serde_json::to_value(&f.values).unwrap();
    v["mode"] = json!("manual");
    assert!(script(p, &mut server(), "job", Some("5"), &fv(v, json!({}))).is_ok());
}

#[test]
fn a_new_event_is_create_event_with_its_schedule_handler_and_comment() {
    let v = fv(
        json!({"name": "ev_new", "mode": "schedule", "enabled": false, "location": "",
               "comment": "a\\b 'c'", "handler": "MESSAGE 'hi' TO CONSOLE;"}),
        json!({"schedules": [{"name": "", "window": "between", "start_time": "08:00", "end_time": "18:00",
                              "every": 15, "unit": "MINUTES", "on": "week", "week": ["Monday", "Friday"],
                              "start_date": "2026-11-20"}]}),
    );
    assert_eq!(
        script(preset("sqlanywhere"), &mut server(), "job", None, &v).unwrap(),
        "-- Evento «ev_new» de SQL Anywhere
CREATE EVENT \"ev_new\"
SCHEDULE \"ev_new\" BETWEEN '08:00:00' AND '18:00:00' EVERY 15 MINUTES ON ('Monday', 'Friday') START DATE '2026-11-20'
DISABLE
HANDLER
BEGIN
MESSAGE 'hi' TO CONSOLE;
END
go
COMMENT ON EVENT \"ev_new\" IS 'a\\\\b ''c'''
go
"
    );
    let mut f = server().rows(
        "WHERE event_name = ?",
        &["event_id"],
        vec![vec![Some("31")]],
    );
    assert_eq!(
        save(preset("sqlanywhere"), &mut f, "job", None, &v)
            .unwrap()
            .as_deref(),
        Some("31")
    );
    assert_eq!(f.log.last().unwrap().1, ["ev_new"]);
}

#[test]
fn a_system_event_with_conditions_and_a_manual_one() {
    let v = fv(
        json!({"name": "ev_grow", "mode": "type", "event_type": "growdb", "location": "REMOTE",
               "handler": "BEGIN\n  MESSAGE 'grow' TO CONSOLE;\nEND"}),
        json!({"conditions": [{"condition": "DBSize", "op": ">", "value": 1000},
                               {"condition": "Interval", "op": ">=", "value": "300"}]}),
    );
    assert_eq!(
        script(preset("sqlanywhere"), &mut server(), "job", None, &v).unwrap(),
        "-- Evento «ev_grow» de SQL Anywhere
CREATE EVENT \"ev_grow\"
TYPE GrowDB
WHERE event_condition('DBSize') > 1000 AND event_condition('Interval') >= 300
AT REMOTE
HANDLER
BEGIN
  MESSAGE 'grow' TO CONSOLE;
END
go
"
    );
    let v = fv(json!({"name": "ev_manual", "mode": "manual"}), json!({}));
    assert_eq!(
        script(preset("sqlanywhere"), &mut server(), "job", None, &v).unwrap(),
        "-- Evento «ev_manual» de SQL Anywhere\nCREATE EVENT \"ev_manual\"\ngo\n"
    );
}

#[test]
fn an_edit_is_one_alter_event_per_change() {
    let f = form_of("5");
    well_formed(&f);
    assert_eq!(val(&f, "mode"), "schedule");
    assert_eq!(val(&f, "comment"), "it's nightly");
    let rows = &f.sections[2].lists[0].rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["window"], "between");
    assert_eq!(rows[1]["every"], 30);
    assert_eq!(rows[1]["unit"], "MINUTES");
    assert_eq!(rows[1]["week"], json!(["Monday", "Friday"]));
    assert_eq!(
        script(
            preset("sqlanywhere"),
            &mut server(),
            "job",
            Some("5"),
            &edit(&f, json!({}))
        )
        .unwrap(),
        "-- «ev_nightly»: no hay cambios que guardar.\n"
    );
    let mut nightly = rows[0].clone();
    nightly.insert("start_time".into(), json!("04:00:00"));
    let v = edit(
        &f,
        json!({
            "enabled": false, "comment": null, "handler": null,
            "list:schedules": [nightly, {"name": "weekend", "window": "start", "start_time": "06:00",
                                          "on": "week", "week": ["Saturday", "Sunday"]}]
        }),
    );
    assert_eq!(
        script(preset("sqlanywhere"), &mut server(), "job", Some("5"), &v).unwrap(),
        "-- Evento «ev_nightly» de SQL Anywhere
ALTER EVENT \"ev_nightly\" DELETE SCHEDULE \"office\"
go
ALTER EVENT \"ev_nightly\" MODIFY SCHEDULE \"nightly\" START TIME '04:00:00' START DATE '2026-11-12'
go
ALTER EVENT \"ev_nightly\" ADD SCHEDULE \"weekend\" START TIME '06:00:00' ON ('Saturday', 'Sunday')
go
ALTER EVENT \"ev_nightly\" DISABLE
go
ALTER EVENT \"ev_nightly\" DELETE HANDLER
go
COMMENT ON EVENT \"ev_nightly\" IS NULL
go
"
    );
}

#[test]
fn switching_what_fires_it_removes_the_old_firing_first() {
    let f = form_of("5");
    let v = edit(
        &f,
        json!({"mode": "type", "event_type": "GrowDB",
               "list:conditions": [{"condition": "DBSize", "op": ">", "value": 1000}]}),
    );
    assert_eq!(
        script(preset("sqlanywhere"), &mut server(), "job", Some("5"), &v).unwrap(),
        "-- Evento «ev_nightly» de SQL Anywhere
ALTER EVENT \"ev_nightly\" DELETE SCHEDULE \"nightly\"
go
ALTER EVENT \"ev_nightly\" DELETE SCHEDULE \"office\"
go
ALTER EVENT \"ev_nightly\" TYPE GrowDB WHERE event_condition('DBSize') > 1000
go
"
    );
    let f = form_of("6");
    assert_eq!(val(&f, "event_type"), "DBDiskSpace");
    assert_eq!(val(&f, "enabled"), false);
    let conds = &f.sections[1].lists[0].rows;
    assert_eq!(conds.len(), 2);
    assert_eq!(conds[0]["op"], "<");
    assert_eq!(conds[0]["value"], 20);
    assert!(!f.sections[1].lists[0].read_only);
    let mut first = conds[0].clone();
    first.insert("value".into(), json!(10));
    let v = edit(&f, json!({"list:conditions": [first, conds[1].clone()]}));
    assert_eq!(
        script(preset("sqlanywhere"), &mut server(), "job", Some("6"), &v).unwrap(),
        "-- Evento «ev_space» de SQL Anywhere
ALTER EVENT \"ev_space\" WHERE event_condition('DBFreePercent') < 10 AND event_condition('Interval') >= 600
go
"
    );
    let v = edit(&f, json!({"mode": "manual"}));
    assert!(
        script(preset("sqlanywhere"), &mut server(), "job", Some("6"), &v)
            .unwrap()
            .contains("ALTER EVENT \"ev_space\" DELETE TYPE")
    );
}

#[test]
fn a_new_name_or_place_drops_and_creates_the_event() {
    let f = form_of("6");
    let s = script(
        preset("sqlanywhere"),
        &mut server(),
        "job",
        Some("6"),
        &edit(&f, json!({"name": "ev_disk"})),
    )
    .unwrap();
    assert_eq!(
        s,
        "-- Evento «ev_space» de SQL Anywhere
-- SQL Anywhere no cambia el nombre ni el lugar (AT) de un evento: se borra y se crea de nuevo
-- recibe otro id.
DROP EVENT \"ev_space\"
go
CREATE EVENT \"ev_disk\"
TYPE DBDiskSpace
WHERE event_condition('DBFreePercent') < 20 AND event_condition('Interval') >= 600
DISABLE
HANDLER
BEGIN MESSAGE 'low' TO CONSOLE; END
go
"
    );
    let s = script(
        preset("sqlanywhere"),
        &mut server(),
        "job",
        Some("5"),
        &edit(&form_of("5"), json!({"location": "REMOTE"})),
    )
    .unwrap();
    assert!(
        s.contains("DROP EVENT \"ev_nightly\"") && s.contains("\nAT REMOTE\n"),
        "{s}"
    );
    assert!(s.contains("COMMENT ON EVENT \"ev_nightly\" IS 'it''s nightly'"));
    assert_eq!(
        delete_script(preset("sqlanywhere"), &mut server(), "job", "5").unwrap(),
        "-- Borra el evento «ev_nightly» de SQL Anywhere\nDROP EVENT \"ev_nightly\"\ngo\n"
    );
}

#[test]
fn conditions_that_are_not_pairs_stay_as_they_are() {
    assert_eq!(
        e::parse_conditions("event_condition('LogSize') <> 5"),
        Some(vec![("LogSize".into(), "!=".into(), "5".into())])
    );
    assert_eq!(e::parse_conditions("event_condition('Nope') < 5"), None);
    assert_eq!(e::parse_conditions("event_condition('DBSize') < x"), None);
    assert_eq!(e::parse_conditions("1 = 1"), None);
    let mut f = server().rows_with(
        "WHERE E.event_id = ?",
        "7",
        EV_COLS,
        vec![vec![
            Some("7"),
            Some("ev_odd"),
            Some("Y"),
            None,
            Some("event_condition('ErrorNumber') IN (1, 2)"),
            None,
            None,
            None,
            Some("RAISERROR"),
        ]],
    );
    let form = form(preset("sqlanywhere"), &mut f, &ctx(), "job", Some("7")).unwrap();
    assert!(
        form.notes[0].contains("no se pudieron leer"),
        "{:?}",
        form.notes
    );
    assert!(form.sections[1].lists[0].read_only);
    assert_eq!(
        val(&form, "raw_condition"),
        "event_condition('ErrorNumber') IN (1, 2)"
    );
    // Untouched, it is left alone; a new name carries it over as it is.
    let v = edit(&form, json!({"enabled": false}));
    assert_eq!(
        script(preset("sqlanywhere"), &mut f, "job", Some("7"), &v).unwrap(),
        "-- Evento «ev_odd» de SQL Anywhere\nALTER EVENT \"ev_odd\" DISABLE\ngo\n"
    );
    let v = edit(&form, json!({"name": "ev_odd2"}));
    assert!(script(preset("sqlanywhere"), &mut f, "job", Some("7"), &v)
        .unwrap()
        .contains("WHERE event_condition('ErrorNumber') IN (1, 2)"));
}

#[test]
fn bad_values_are_refused() {
    let types = vec![("GrowDB".to_string(), "GrowDB".to_string())];
    for (vals, lists, want) in [
        (
            json!({"name": "e", "mode": "type", "event_type": "Nope"}),
            json!({}),
            "no es un evento del sistema",
        ),
        (
            json!({"name": "e", "mode": "type", "event_type": "GrowDB"}),
            json!({"conditions": [{"condition": "DBSize", "op": "<", "value": "1; drop"}]}),
            "tiene que ser un número",
        ),
        (
            json!({"name": "e", "mode": "type", "event_type": "GrowDB"}),
            json!({"conditions": [{"condition": "Hack", "op": "<", "value": 1}]}),
            "«Condición»",
        ),
        (
            json!({"name": "e", "mode": "schedule"}),
            json!({"schedules": []}),
            "al menos un calendario",
        ),
        (
            json!({"name": "e", "mode": "schedule"}),
            json!({"schedules": [{"name": "", "start_time": "01:00"}, {"name": "b", "start_time": "02:00"}]}),
            "cada uno lleva su nombre",
        ),
        (
            json!({"name": "e", "mode": "schedule"}),
            json!({"schedules": [{"name": "a", "start_time": "25:00"}]}),
            "Calendario 1: «Hora de inicio»",
        ),
        (
            json!({"name": "e", "mode": "schedule"}),
            json!({"schedules": [{"name": "a", "window": "between", "start_time": "01:00"}]}),
            "«Hora de fin»",
        ),
        (
            json!({"name": "e", "mode": "schedule"}),
            json!({"schedules": [{"name": "a", "start_time": "01:00", "on": "month", "month": ["40"]}]}),
            "no es un día del mes",
        ),
        (
            json!({"name": "e", "mode": "x"}),
            json!({}),
            "«Qué lo dispara»",
        ),
        (
            json!({"name": "e", "mode": "manual", "location": "MARS"}),
            json!({}),
            "«Dónde se dispara (AT)»",
        ),
        (
            json!({"name": "a\nb", "mode": "manual"}),
            json!({}),
            "«Nombre»",
        ),
    ] {
        let m = refused(e::from_values(&fv(vals.clone(), lists), &types));
        assert!(m.contains(want), "{vals}: {m}");
    }
}

#[test]
fn info_and_the_rest() {
    let i = info(preset("sqlanywhere"), &mut server(), &ctx()).unwrap();
    assert_eq!(i.kinds[0].key, "job");
    assert!(i.notes[0].contains("MANAGE ANY EVENT"));
    let f = form(preset("sqlanywhere"), &mut server(), &ctx(), "job", None).unwrap();
    well_formed(&f);
    assert_eq!(
        f.choices["event_type"],
        [
            ("DBDiskSpace".to_string(), "DBDiskSpace".to_string()),
            ("GrowDB".to_string(), "GrowDB".to_string()),
            ("RAISERROR".to_string(), "RAISERROR".to_string())
        ]
    );
    assert!(matches!(
        clear_history(preset("sqlanywhere"), &mut server(), "5"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        logs(preset("sqlanywhere"), &mut server()),
        Err(Error::Unsupported(_))
    ));
}

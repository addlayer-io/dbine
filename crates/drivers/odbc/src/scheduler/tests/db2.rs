//! Db2 LUW's Administrative Task Scheduler.

use super::*;
use crate::scheduler::db2 as e;

const NOW_COLS: &[&str] = &["LOCAL_NOW", "UTC_NOW"];
const COLS: &[&str] = &[
    "NAME",
    "TASKID",
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
];

/// The server at 2026-11-12 09:00 local, task 12 "nightly" whose begin
/// has passed.
fn server() -> Fake {
    Fake::default()
        .rows(
            "LOCAL_NOW",
            NOW_COLS,
            vec![vec![
                Some("2026-11-12 09:00:00.000000"),
                Some("2026-11-12 12:00:00.000000"),
            ]],
        )
        .rows(
            "WHERE T.TASKID = 12",
            COLS,
            vec![vec![
                Some("nightly"),
                Some("12"),
                Some("ANA"),
                Some("U"),
                Some("2026-01-01-00.00.00.000000"),
                Some("2027-01-01-00.00.00.000000"),
                Some("100"),
                Some("0 3 * * *"),
                Some("SYSPROC"),
                Some("ADMIN_CMD"),
                Some("VALUES ('RUNSTATS ON TABLE APP.T')"),
                None,
                Some("2026-11-01-10.00.00.000000"),
                Some("estadísticas"),
            ]],
        )
        .rows(
            "SYSCAT.ROUTINES",
            &["ROUTINESCHEMA", "ROUTINENAME"],
            vec![vec![Some("SYSPROC"), Some("ADMIN_CMD")]],
        )
}

fn values(over: Value) -> FormValues {
    let mut v = json!({
        "name": "nightly",
        "remarks": "estadísticas",
        "procedure_schema": "SYSPROC",
        "procedure_name": "ADMIN_CMD",
        "procedure_input": "VALUES ('RUNSTATS ON TABLE APP.T')",
        "schedule": "0 3 * * *",
        "begin": "2026-01-01T00:00:00",
        "end": "2027-01-01T00:00:00",
        "max_invocations": 100,
        "options": null
    });
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({}))
}

fn script(id: Option<&str>, v: &FormValues) -> Result<String> {
    super::script(preset("db2"), &mut server(), "job", id, v)
}

#[test]
fn a_new_task_is_admin_task_add_with_every_argument_as_a_literal() {
    let v = fv(
        json!({
            "name": "it's nightly", "remarks": "a 'b'", "procedure_schema": "sysproc",
            "procedure_name": "admin_cmd", "procedure_input": "VALUES ('RUNSTATS ON TABLE X')",
            "schedule": "0  3 * * *", "begin": "2026-12-01T00:00:00", "max_invocations": 10
        }),
        json!({}),
    );
    assert_eq!(
        script(None, &v).unwrap(),
        "-- Tarea «it's nightly» del planificador de tareas de Db2
CALL SYSPROC.ADMIN_TASK_ADD(
    'it''s nightly',
    TIMESTAMP('2026-12-01-00.00.00.000000'),
    NULL,
    10,
    '0 3 * * *',
    'SYSPROC',
    'ADMIN_CMD',
    'VALUES (''RUNSTATS ON TABLE X'')',
    NULL,
    'a ''b'''
);
"
    );
}

#[test]
fn a_procedure_the_catalog_lacks_is_refused() {
    // The catalog answers no routine of that name.
    let mut f = Fake::default().rows(
        "LOCAL_NOW",
        NOW_COLS,
        vec![vec![
            Some("2026-11-12 09:00:00"),
            Some("2026-11-12 12:00:00"),
        ]],
    );
    let v = values(json!({"begin": null, "procedure_name": "NOPE"}));
    let m = refused(super::script(preset("db2"), &mut f, "job", None, &v));
    assert_eq!(m, "No existe el procedimiento «SYSPROC.NOPE».");
}

#[test]
fn times_in_the_past_and_an_end_before_the_begin_are_refused() {
    let past_begin = values(json!({"name": "n2", "begin": "2026-11-01T00:00:00"}));
    assert!(refused(script(None, &past_begin)).contains("«Inicio» ya pasó"));
    let past_end = values(json!({"name": "n2", "begin": null, "end": "2026-11-12T08:59:00"}));
    assert!(refused(script(None, &past_end)).contains("«Fin» ya pasó"));
    let backwards = values(json!({"begin": "2027-02-01T00:00:00", "end": "2027-01-01T00:00:00"}));
    assert_eq!(
        refused(script(None, &backwards)),
        "«Fin» tiene que ser posterior a «Inicio»."
    );
}

#[test]
fn a_schedule_change_is_admin_task_update_in_place_with_a_passed_begin_as_null() {
    let v = values(json!({"schedule": "30 4 * * 1-5", "max_invocations": null, "remarks": ""}));
    let s = script(Some("12"), &v).unwrap();
    assert_eq!(
        s,
        "-- Tarea «nightly» del planificador de tareas de Db2
-- Inicio original: 2026-01-01 00:00:00. Db2 no acepta un inicio pasado: sin él, la tarea corre desde ya según su calendario.
CALL SYSPROC.ADMIN_TASK_UPDATE(
    'nightly',
    NULL,
    TIMESTAMP('2027-01-01-00.00.00.000000'),
    NULL,
    '30 4 * * 1-5',
    NULL,
    NULL
);
"
    );
}

#[test]
fn a_new_name_procedure_or_input_removes_and_adds_the_task_again() {
    for over in [
        json!({"name": "nightly2"}),
        json!({"procedure_input": "VALUES ('RUNSTATS ON TABLE APP.U')"}),
    ] {
        let s = script(Some("12"), &values(over.clone())).unwrap();
        assert!(s.contains("-- Db2 no cambia el nombre"), "{s}");
        let remove = s
            .find("CALL SYSPROC.ADMIN_TASK_REMOVE(\n    'nightly',\n    NULL\n)")
            .unwrap();
        let add = s.find("CALL SYSPROC.ADMIN_TASK_ADD(").unwrap();
        assert!(remove < add, "{s}");
        assert!(!s.contains("ADMIN_TASK_UPDATE"));
    }
}

#[test]
fn an_unchanged_task_has_nothing_to_save() {
    assert_eq!(
        script(Some("12"), &values(json!({}))).unwrap(),
        "-- «nightly»: no hay cambios que guardar.\n"
    );
    // The same cron with other spaces is the same.
    assert_eq!(
        script(Some("12"), &values(json!({"schedule": " 0 3  * * * "}))).unwrap(),
        "-- «nightly»: no hay cambios que guardar.\n"
    );
}

#[test]
fn delete_and_clear_history_remove_the_task_or_only_its_finished_status() {
    let mut f = server();
    assert_eq!(
        delete_script(preset("db2"), &mut f, "job", "12").unwrap(),
        "-- Borra la tarea «nightly» del planificador de tareas de Db2 y sus registros de estado
CALL SYSPROC.ADMIN_TASK_REMOVE(
    'nightly',
    NULL
);
"
    );
    let mut f = server();
    clear_history(preset("db2"), &mut f, "12").unwrap();
    assert!(f.sent("CALL SYSPROC.ADMIN_TASK_REMOVE(\n    'nightly',\n    CURRENT TIMESTAMP\n)"));
}

#[test]
fn forged_ids_never_reach_the_server() {
    for id in ["12; DROP TABLE x", "' OR 1=1 --", "", "-1", "1e3"] {
        let mut f = server();
        assert!(form(preset("db2"), &mut f, &ctx(), "job", Some(id)).is_err());
        assert!(delete_script(preset("db2"), &mut f, "job", id).is_err());
        assert!(!f.sent("TASKID ="), "{id}");
    }
}

#[test]
fn the_form_of_a_task_has_its_values_and_a_new_one_its_defaults() {
    let mut f = server();
    let form = form(preset("db2"), &mut f, &ctx(), "job", Some("12")).unwrap();
    well_formed(&form);
    assert_eq!(val(&form, "name"), "nightly");
    assert_eq!(val(&form, "begin"), "2026-01-01T00:00:00");
    assert_eq!(val(&form, "max_invocations"), 100);
    assert_eq!(val(&form, "taskid"), "12");
    assert_eq!(val(&form, "owner"), "ANA");
    assert!(form.notes.iter().any(|n| n.contains("se borra y se crea")));
    let new = super::form(preset("db2"), &mut Fake::default(), &ctx(), "job", None).unwrap();
    well_formed(&new);
    assert_eq!(val(&new, "procedure_schema"), "SYSPROC");
    assert_eq!(val(&new, "begin"), Value::Null);
}

#[test]
fn a_save_runs_in_a_transaction_and_answers_the_new_taskid() {
    let mut f = server().rows(
        "SELECT TASKID FROM SYSTOOLS.ADMIN_TASK_LIST WHERE NAME = ?",
        &["TASKID"],
        vec![vec![Some("31")]],
    );
    let v = values(json!({"name": "otra", "begin": null}));
    let id = save(preset("db2"), &mut f, "job", None, &v).unwrap();
    assert_eq!(id.as_deref(), Some("31"));
    assert_eq!(f.tx, ["begin", "commit"]);
    let looked = f
        .log
        .iter()
        .find(|(s, _)| s.contains("WHERE NAME = ?"))
        .unwrap();
    assert_eq!(looked.1, ["otra"]);
    // An edit keeps the id.
    let mut f = server();
    let v = values(json!({"schedule": "0 4 * * *"}));
    assert_eq!(
        save(preset("db2"), &mut f, "job", Some("12"), &v)
            .unwrap()
            .as_deref(),
        Some("12")
    );
    assert!(f.sent("ADMIN_TASK_UPDATE"));
}

#[test]
fn bad_values_are_refused_with_the_field_named() {
    for (over, want) in [
        (json!({"name": ""}), "Falta «Nombre»."),
        (
            json!({"name": "x".repeat(129)}),
            "«Nombre» admite hasta 128 caracteres.",
        ),
        (json!({"schedule": "every day"}), "«Calendario»"),
        (
            json!({"max_invocations": -1}),
            "«Máximo de ejecuciones» tiene que estar entre 0",
        ),
        (
            json!({"remarks": "ñ".repeat(200)}),
            "«Comentario» admite hasta 254 bytes.",
        ),
        (json!({"begin": "ayer"}), "«Inicio»"),
    ] {
        let m = refused(e::from_values(&values(over.clone())));
        assert!(m.contains(want), "{over}: {m}");
    }
}

#[test]
fn info_says_when_the_scheduler_is_off_or_not_yet_created() {
    let mut f = Fake::default()
        .rows("SYSCAT.TABLES", &["TABNAME"], vec![])
        .rows("DB2_ATS_ENABLE", &["REG_VAR_VALUE"], vec![vec![Some("NO")]]);
    let i = info(preset("db2"), &mut f, &ctx()).unwrap();
    assert_eq!(i.kinds.len(), 1);
    assert_eq!(i.kinds[0].key, "job");
    assert!(i.kinds[0].can_create && i.kinds[0].can_edit && i.kinds[0].can_delete);
    assert!(!i.logs);
    assert!(i.notes.iter().any(|n| n.contains("primera llamada")));
    assert!(i.notes.iter().any(|n| n.contains("DB2_ATS_ENABLE")));
}

#[test]
fn the_log_is_the_status_records_newest_first() {
    let mut f = Fake::default().rows(
        "ADMIN_TASK_STATUS S ORDER BY",
        &[
            "NAME",
            "TASKID",
            "STATUS",
            "INVOCATION",
            "BEGIN_TIME",
            "END_TIME",
            "SQLCODE",
            "SQLSTATE",
            "SQLERRMC",
            "RC",
        ],
        vec![vec![
            Some("nightly"),
            Some("12"),
            Some("COMPLETE"),
            Some("3"),
            Some("2026-11-12-03.00.00.000000"),
            Some("2026-11-12-03.00.05.000000"),
            Some("-204"),
            Some("42704"),
            Some("4150502E54"),
            Some("0"),
        ]],
    );
    let t = logs(preset("db2"), &mut f).unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].columns.len(), t[0].rows[0].len());
    assert_eq!(t[0].rows[0][0], "nightly");
    assert_eq!(t[0].rows[0][3], "2026-11-12-03.00.00");
    assert_eq!(t[0].rows[0][7], "APP.T");
    // Before the first task the view isn't there: an empty table.
    let mut f = Fake::default().fail("ADMIN_TASK_STATUS", Some("-204"));
    assert!(logs(preset("db2"), &mut f).unwrap()[0].rows.is_empty());
}

#[test]
fn the_tree_lists_the_jobs_ids_without_an_enabled_state() {
    let mut f = server()
        .rows(
            "SYSCAT.TABLES",
            &["TABNAME"],
            vec![
                vec![Some("ADMIN_TASK_LIST")],
                vec![Some("ADMIN_TASK_STATUS")],
            ],
        )
        .rows(
            "FROM SYSTOOLS.ADMIN_TASK_LIST T",
            COLS,
            vec![vec![
                Some("nightly"),
                Some("12"),
                Some("ANA"),
                None,
                None,
                None,
                None,
                Some("0 3 * * *"),
                Some("SYSPROC"),
                Some("ADMIN_CMD"),
                None,
                None,
                None,
                Some("r"),
            ]],
        );
    let l = items(preset("db2"), &mut f, &ctx(), "job").unwrap();
    assert_eq!(l.items.len(), 1);
    assert_eq!(l.items[0].id, "12");
    assert_eq!(l.items[0].enabled, None);
}

//! Informix and GBase 8s: the DB Scheduler in sysadmin.

use super::*;
use crate::scheduler::informix as e;

const COLS: &[&str] = &[
    "tk_id",
    "tk_name",
    "tk_type",
    "tk_group",
    "tk_description",
    "tk_dbs",
    "tk_execute",
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
    "tk_total_executions",
    "tk_total_time",
    "tk_sequence",
    "tk_attributes",
    "dbine_start",
    "dbine_stop",
    "dbine_frequency",
    "dbine_next",
    "dbine_delete",
];

/// [`base`] with a login that is DBA of sysadmin (it may write).
fn server() -> Fake {
    server_as("mon_users")
}

/// [`server`] with task 7 named `name` (`my_users`: a user's, not one
/// Informix ships).
fn server_as(name: &'static str) -> Fake {
    base_as(name).rows("sysadmin:sysusers", &["usertype"], vec![vec![Some("D")]])
}

/// Task 7, `mon_users`: a built-in sensor every 4 hours on weekdays, with
/// one parameter; groups MISC and PERFORMANCE; databases sysadmin and
/// ventas. The login's privileges aren't answered.
fn base() -> Fake {
    base_as("mon_users")
}

fn base_as(name: &'static str) -> Fake {
    Fake::default()
        .rows(
            "WHERE t.tk_id = 7",
            COLS,
            vec![vec![
                Some("7"),
                Some(name),
                Some("SENSOR"),
                Some("PERFORMANCE"),
                Some("Usuarios"),
                Some("sysadmin"),
                Some("INSERT INTO mon_users SELECT $DATA_SEQ_ID, * FROM sysmaster:sysrstcb"),
                Some("t"),
                Some("t"),
                Some("t"),
                Some("t"),
                Some("t"),
                Some("f"),
                Some("f"),
                Some("t"),
                Some("0"),
                Some("mon_users"),
                Some("CREATE TABLE mon_users (ID integer)"),
                Some("42"),
                Some("1.5"),
                Some("43"),
                Some("0"),
                Some("08:00:00"),
                None,
                Some(" 0 04:00:00"),
                Some("2026-11-12 12:00:00"),
                Some(" 7 00:00:00"),
            ]],
        )
        .rows(
            "ph_threshold WHERE task_name = ?",
            &["name", "task_name", "value", "value_type", "description"],
            vec![vec![
                Some("MAX_USERS"),
                Some(name),
                Some("100"),
                Some("NUMERIC"),
                Some("tope"),
            ]],
        )
        .rows(
            "sysadmin:ph_group ORDER BY",
            &["group_name", "group_description"],
            vec![
                vec![Some("MISC"), Some("varios")],
                vec![Some("PERFORMANCE"), None],
            ],
        )
        .rows(
            "sysmaster:sysdatabases",
            &["name"],
            vec![vec![Some("sysadmin")], vec![Some("ventas")]],
        )
}

fn new_task(over: Value, lists: Value) -> FormValues {
    let mut v = json!({
        "name": "purge_o'neil", "type": "TASK", "group": "misc", "description": "borra\nviejo",
        "enabled": true, "priority": 2, "database": "ventas",
        "execute": "DELETE FROM log WHERE fecha < TODAY - 30",
        "start_time": "02:00", "stop_time": null, "frequency": 86400,
        "days": ["mon", "wed"], "next_execution": null
    });
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, lists)
}

fn mon_users(over: Value, thresholds: Value) -> FormValues {
    task7("mon_users", over, thresholds)
}

/// Task 7's form (named `name`) with `over` on top.
fn task7(name: &'static str, over: Value, thresholds: Value) -> FormValues {
    let f = form(
        preset("informix"),
        &mut server_as(name),
        &ctx(),
        "job",
        Some("7"),
    )
    .unwrap();
    let mut v = serde_json::to_value(&f.values).unwrap();
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({ "thresholds": thresholds }))
}

fn kept() -> Value {
    json!([{"name": "MAX_USERS", "value": "100", "value_type": "NUMERIC", "description": "tope"}])
}

fn edit(v: &FormValues) -> String {
    edit_as("mon_users", v)
}

fn edit_as(name: &'static str, v: &FormValues) -> String {
    script(
        preset("informix"),
        &mut server_as(name),
        "job",
        Some("7"),
        v,
    )
    .unwrap()
}

#[test]
fn a_new_task_is_one_insert_with_every_column_and_its_parameters() {
    let v = new_task(
        json!({}),
        json!({"thresholds": [{"name": "DAYS", "value": "30", "value_type": "numeric", "description": ""}]}),
    );
    let s = script(preset("informix"), &mut server(), "job", None, &v).unwrap();
    assert_eq!(
        s,
        "-- Tarea «purge_o'neil» del planificador de Informix (sysadmin)
EXECUTE PROCEDURE IFX_ALLOW_NEWLINE('t');
INSERT INTO sysadmin:ph_task (
    tk_name,
    tk_type,
    tk_group,
    tk_description,
    tk_dbs,
    tk_execute,
    tk_start_time,
    tk_stop_time,
    tk_frequency,
    tk_monday,
    tk_tuesday,
    tk_wednesday,
    tk_thursday,
    tk_friday,
    tk_saturday,
    tk_sunday,
    tk_enable,
    tk_priority,
    tk_result_table,
    tk_create
) VALUES (
    'purge_o''neil',
    'TASK',
    'MISC',
    'borra
viejo',
    'ventas',
    'DELETE FROM log WHERE fecha < TODAY - 30',
    DATETIME(02:00:00) HOUR TO SECOND,
    NULL,
    INTERVAL(1 00:00:00) DAY TO SECOND,
    't',
    'f',
    't',
    'f',
    'f',
    'f',
    'f',
    't',
    2,
    NULL,
    NULL
);
INSERT INTO sysadmin:ph_threshold (name, task_name, value, value_type, description) VALUES ('DAYS', 'purge_o''neil', '30', 'NUMERIC', '');
"
    );
}

#[test]
fn a_save_is_one_transaction_and_answers_the_new_tk_id() {
    let mut f = server().rows(
        "SELECT MAX(tk_id) AS tk_id FROM sysadmin:ph_task WHERE tk_name = ?",
        &["tk_id"],
        vec![vec![Some("88")]],
    );
    let v = new_task(json!({"description": "una línea"}), json!({}));
    assert_eq!(
        save(preset("informix"), &mut f, "job", None, &v)
            .unwrap()
            .as_deref(),
        Some("88")
    );
    assert_eq!(f.tx, ["begin", "commit"]);
    assert!(!f.sent("IFX_ALLOW_NEWLINE"));
    // A name another task has.
    let mut f = server().rows(
        "SELECT tk_id FROM sysadmin:ph_task WHERE tk_name = ?",
        &["tk_id"],
        vec![vec![Some("3")]],
    );
    assert!(refused(save(preset("informix"), &mut f, "job", None, &v)).contains("Ya hay una tarea"));
    assert!(!f.sent("INSERT"));
}

#[test]
fn an_edit_updates_only_the_columns_that_changed() {
    let v = mon_users(
        json!({"frequency": 7200, "days": ["mon", "tue", "wed", "thu", "fri", "sat"], "enabled": false}),
        kept(),
    );
    assert_eq!(
        edit(&v),
        "-- Tarea «mon_users» del planificador de Informix (sysadmin)
UPDATE sysadmin:ph_task SET
    tk_frequency = INTERVAL(0 02:00:00) DAY TO SECOND,
    tk_saturday = 't',
    tk_enable = 'f'
 WHERE tk_id = 7;
"
    );
    assert_eq!(
        edit(&mon_users(json!({}), kept())),
        "-- «mon_users»: no hay cambios que guardar.\n"
    );
    let v = mon_users(
        json!({"next_execution": "2026-11-13T06:00:00", "keep": null}),
        kept(),
    );
    let s = edit(&v);
    assert!(
        s.contains("tk_next_execution = DATETIME(2026-11-13 06:00:00) YEAR TO SECOND"),
        "{s}"
    );
    assert!(s.contains("tk_delete = NULL"), "{s}");
}

#[test]
fn a_new_name_moves_its_parameters_and_their_changes_follow() {
    let v = task7(
        "my_users",
        json!({"name": "my_users2"}),
        json!([
            {"name": "MAX_USERS", "value": "200", "value_type": "NUMERIC", "description": "tope"},
            {"name": "MIN_USERS", "value": "1", "value_type": "NUMERIC", "description": ""}
        ]),
    );
    let s = edit_as("my_users", &v);
    let lines: Vec<&str> = s.lines().collect();
    assert!(lines.contains(&"    tk_name = 'my_users2'"), "{s}");
    let moved = s
        .find("UPDATE sysadmin:ph_threshold SET task_name = 'my_users2' WHERE task_name = 'my_users';")
        .unwrap();
    let changed = s
        .find("UPDATE sysadmin:ph_threshold SET value = '200', value_type = 'NUMERIC', description = 'tope' WHERE task_name = 'my_users2' AND name = 'MAX_USERS';")
        .unwrap();
    let added = s
        .find("INSERT INTO sysadmin:ph_threshold (name, task_name, value, value_type, description) VALUES ('MIN_USERS', 'my_users2', '1', 'NUMERIC', '');")
        .unwrap();
    assert!(moved < changed && changed < added, "{s}");
    // A task Informix ships keeps its name (its form has it read-only).
    let m = refused(script(
        preset("informix"),
        &mut server(),
        "job",
        Some("7"),
        &mon_users(json!({"name": "mon_users2"}), kept()),
    ));
    assert!(m.contains("no se borran ni cambian de nombre"), "{m}");
    // A parameter taken out is deleted.
    let s = edit(&mon_users(json!({}), json!([])));
    assert!(s.contains(
        "DELETE FROM sysadmin:ph_threshold WHERE task_name = 'mon_users' AND name = 'MAX_USERS';"
    ));
}

#[test]
fn a_startup_task_keeps_the_schedule_the_form_hides() {
    let v = mon_users(
        json!({"type": "STARTUP SENSOR", "start_time": null, "frequency": null}),
        kept(),
    );
    assert_eq!(
        edit(&v),
        "-- Tarea «mon_users» del planificador de Informix (sysadmin)
UPDATE sysadmin:ph_task SET
    tk_type = 'STARTUP SENSOR'
 WHERE tk_id = 7;
"
    );
    // A sensor that becomes a task keeps its table columns.
    let s = edit(&mon_users(
        json!({"type": "TASK", "result_table": null}),
        kept(),
    ));
    assert!(
        s.contains("tk_type = 'TASK'") && !s.contains("tk_result_table"),
        "{s}"
    );
}

#[test]
fn delete_takes_its_parameters_and_runs_and_clear_only_the_runs() {
    let mut f = server_as("my_users");
    assert_eq!(
        delete_script(preset("informix"), &mut f, "job", "7").unwrap(),
        "-- Borra la tarea «my_users» del planificador de Informix, con sus parámetros y sus ejecuciones (las alertas quedan)
DELETE FROM sysadmin:ph_threshold WHERE task_name = 'my_users';
DELETE FROM sysadmin:ph_run WHERE run_task_id = 7;
DELETE FROM sysadmin:ph_task WHERE tk_id = 7;
"
    );
    let mut f = server_as("my_users");
    delete(preset("informix"), &mut f, "job", "7").unwrap();
    assert_eq!(f.tx, ["begin", "commit"]);
    // A task Informix ships isn't deleted (it's disabled instead).
    let mut f = server();
    let m = refused(delete_script(preset("informix"), &mut f, "job", "7"));
    assert!(m.contains("se deshabilitan"), "{m}");
    assert!(delete(preset("informix"), &mut server(), "job", "7").is_err());
    // Its runs are cleared like any task's.
    let mut f = server();
    clear_history(preset("informix"), &mut f, "7").unwrap();
    assert!(f.sent("DELETE FROM sysadmin:ph_run WHERE run_task_id = 7"));
    assert!(!f.sent("ph_task WHERE"));
    for id in ["7; DELETE FROM ph_task", "x", ""] {
        let mut f = server();
        assert!(delete_script(preset("informix"), &mut f, "job", id).is_err());
        assert!(f.log.is_empty());
    }
}

#[test]
fn bad_values_are_refused() {
    for (over, want) in [
        (json!({"name": "x".repeat(37)}), "«Nombre» admite hasta 36"),
        (json!({"type": "TRIGGER"}), "«Tipo»"),
        (json!({"group": "NOPE"}), "No existe el grupo «NOPE»."),
        (json!({"database": "otra"}), "No existe la base «otra»."),
        (
            json!({"frequency": 100 * 86400}),
            "«Cada» tiene que estar entre 1",
        ),
        (json!({"days": ["lunes"]}), "no es un día de la semana"),
        (json!({"execute": "  "}), "Falta «Ejecuta»."),
        (json!({"start_time": "2am"}), "«Desde»"),
        (json!({"priority": -1}), "«Prioridad»"),
    ] {
        let m = refused(script(
            preset("informix"),
            &mut server(),
            "job",
            None,
            &new_task(over.clone(), json!({})),
        ));
        assert!(m.contains(want), "{over}: {m}");
    }
    let dup = json!({"thresholds": [{"name": "A"}, {"name": "A"}]});
    let m = refused(script(
        preset("informix"),
        &mut server(),
        "job",
        None,
        &new_task(json!({}), dup),
    ));
    assert!(m.contains("está repetido"), "{m}");
}

#[test]
fn intervals_read_and_write_back() {
    assert_eq!(e::interval_secs(" 1 00:00:00"), Some(86_400));
    assert_eq!(e::interval_secs("0 00:30:00.00000"), Some(1800));
    assert_eq!(e::interval_secs("04:00:00"), Some(14_400));
    assert_eq!(e::interval_secs("x"), None);
    assert_eq!(
        e::interval_literal(90_061),
        "INTERVAL(1 01:01:01) DAY TO SECOND"
    );
}

#[test]
fn the_form_reads_the_task_its_parameters_and_what_to_pick() {
    let f = form(preset("informix"), &mut server(), &ctx(), "job", Some("7")).unwrap();
    well_formed(&f);
    assert_eq!(val(&f, "type"), "SENSOR");
    assert_eq!(val(&f, "frequency"), 14_400);
    assert_eq!(val(&f, "keep"), 7 * 86_400);
    assert_eq!(val(&f, "start_time"), "08:00:00");
    assert_eq!(val(&f, "days"), json!(["mon", "tue", "wed", "thu", "fri"]));
    assert_eq!(val(&f, "next_execution"), "2026-11-12T12:00:00");
    assert_eq!(val(&f, "total_executions"), "42");
    assert_eq!(f.choices["group"].len(), 2);
    assert_eq!(f.choices["database"].len(), 2);
    let params = &f
        .sections
        .iter()
        .find(|s| s.key == "parameters")
        .unwrap()
        .lists[0];
    assert_eq!(params.rows.len(), 1);
    assert!(!params.read_only);
    assert!(f.notes.iter().any(|n| n.contains("tarea del servidor")));
    let new = form(preset("informix"), &mut server(), &ctx(), "job", None).unwrap();
    well_formed(&new);
    assert_eq!(val(&new, "group"), "MISC");
}

#[test]
fn groups_are_created_renamed_with_their_tasks_and_deleted_when_empty() {
    let s = script(
        preset("informix"),
        &mut server(),
        "group",
        None,
        &fv(
            json!({"name": "BACKUPS", "description": "copias"}),
            json!({}),
        ),
    )
    .unwrap();
    assert_eq!(
        s,
        "-- Grupo «BACKUPS» del planificador de Informix
INSERT INTO sysadmin:ph_group (group_name, group_description) VALUES ('BACKUPS', 'copias');
"
    );
    let group = |name: &'static str| {
        move || {
            server()
                .rows(
                    "ph_group WHERE group_name = ?",
                    &["group_name", "group_description"],
                    vec![vec![Some(name), Some("varios")]],
                )
                // mon_users (built in) is in PERFORMANCE; purge (a user's)
                // in LIMPIEZA.
                .rows(
                    "SELECT tk_group, tk_name FROM sysadmin:ph_task",
                    &["tk_group", "tk_name"],
                    vec![
                        vec![Some("PERFORMANCE"), Some("mon_users")],
                        vec![Some("LIMPIEZA"), Some("purge")],
                    ],
                )
        }
    };
    let one = group("LIMPIEZA");
    let s = script(
        preset("informix"),
        &mut one(),
        "group",
        Some("LIMPIEZA"),
        &fv(
            json!({"name": "LIMPIEZA", "description": "otros"}),
            json!({}),
        ),
    )
    .unwrap();
    assert!(s.contains(
        "UPDATE sysadmin:ph_group SET group_description = 'otros' WHERE group_name = 'LIMPIEZA';"
    ));
    let p = e::alter_group_plan(("LIMPIEZA", "varios"), "VARIOS", "varios");
    assert!(p.atomic);
    let s = p.script();
    let ins = s.find("INSERT INTO sysadmin:ph_group (group_name, group_description) VALUES ('VARIOS', 'varios');").unwrap();
    let mv = s
        .find("UPDATE sysadmin:ph_task SET tk_group = 'VARIOS' WHERE tk_group = 'LIMPIEZA';")
        .unwrap();
    let del = s
        .find("DELETE FROM sysadmin:ph_group WHERE group_name = 'LIMPIEZA';")
        .unwrap();
    assert!(ins < mv && mv < del, "{s}");
    // In use: refused; empty: deleted.
    let mut f = one().rows(
        "SELECT tk_name FROM sysadmin:ph_task WHERE tk_group = ?",
        &["tk_name"],
        vec![vec![Some("purge")]],
    );
    assert!(refused(delete_script(
        preset("informix"),
        &mut f,
        "group",
        "LIMPIEZA"
    ))
    .contains("pasalas a otro grupo"));
    assert_eq!(
        delete_script(preset("informix"), &mut one(), "group", "LIMPIEZA").unwrap(),
        "-- Borra el grupo «LIMPIEZA» del planificador de Informix\nDELETE FROM sysadmin:ph_group WHERE group_name = 'LIMPIEZA';\n"
    );
    let f = form(
        preset("informix"),
        &mut one(),
        &ctx(),
        "group",
        Some("LIMPIEZA"),
    )
    .unwrap();
    well_formed(&f);
    assert_eq!(val(&f, "description"), "varios");
    assert!(!f.read_only && f.notes.is_empty());

    // The server's groups (MISC, and one with a built-in task) are only
    // read: not changed, renamed nor deleted, even when empty.
    for name in ["MISC", "PERFORMANCE"] {
        let g = group(name);
        let f = form(preset("informix"), &mut g(), &ctx(), "group", Some(name)).unwrap();
        assert!(
            f.read_only && f.notes[0].contains("grupo del servidor"),
            "{name}"
        );
        let m = refused(script(
            preset("informix"),
            &mut g(),
            "group",
            Some(name),
            &fv(json!({"name": name, "description": "otros"}), json!({})),
        ));
        assert!(m.contains("no se cambian ni se borran"), "{name}: {m}");
        assert!(
            refused(delete_script(preset("informix"), &mut g(), "group", name))
                .contains("no se cambian ni se borran")
        );
        // Unchanged, it's no change at all.
        assert!(script(
            preset("informix"),
            &mut g(),
            "group",
            Some(name),
            &fv(json!({"name": name, "description": "varios"}), json!({})),
        )
        .unwrap()
        .contains("no hay cambios"));
    }
    let l = items(
        preset("informix"),
        &mut group("LIMPIEZA")(),
        &ctx(),
        "group",
    )
    .unwrap();
    assert_eq!(l.items.len(), 2);
    assert_eq!(l.items[0].description.as_deref(), Some("varios"));
    // The fixture lists MISC and PERFORMANCE: both the server's.
    for i in &l.items {
        assert!(i.system, "{}", i.name);
        assert_eq!((i.can_edit, i.can_delete), (Some(false), Some(false)));
        assert_eq!(i.actions, None);
    }
}

#[test]
fn a_task_runs_now_and_built_in_ones_keep_their_name() {
    let p = preset("informix");
    let i = info(p, &mut server(), &ctx()).unwrap();
    let keys: Vec<&str> = i.kinds[0].actions.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, ["run"]);
    assert!(i.kinds[0].clear_history && !i.kinds[0].run_from_step);
    assert!(i.kinds[1].actions.is_empty() && !i.kinds[1].clear_history);
    assert_eq!(
        action_script(p, &mut server(), &ctx(), "job", "7", "run").unwrap(),
        "-- Ejecuta la tarea ahora, en segundo plano (exectask_async), fuera de su programación.\nEXECUTE FUNCTION exectask_async('mon_users');\n"
    );
    let mut f = server();
    action(p, &mut f, &ctx(), "job", "7", "run").unwrap();
    assert!(f.sent("EXECUTE FUNCTION exectask_async('mon_users')"));
    // sysadmin's functions answer only a session connected to sysadmin.
    let other = Ctx {
        database: "ventas".into(),
    };
    assert!(matches!(
        action_script(p, &mut server(), &other, "job", "7", "run"),
        Err(Error::Unsupported(m)) if m.contains("sysadmin")
    ));
    for (kind, act) in [("job", "stop"), ("group", "run")] {
        assert!(matches!(
            action_script(p, &mut server(), &ctx(), kind, "7", act),
            Err(Error::Unsupported(_))
        ));
    }
    // A built-in task: its name is read only, the rest is tuned.
    let f = form(p, &mut server(), &ctx(), "job", Some("7")).unwrap();
    well_formed(&f);
    let name = &f.sections[0].fields[0];
    assert!(name.key == "name" && name.read_only);
    assert!(!f.read_only);
    let s = edit(&mon_users(json!({"frequency": 3600}), kept()));
    assert!(
        s.contains("tk_frequency = INTERVAL(0 01:00:00) DAY TO SECOND"),
        "{s}"
    );
    let f = form(p, &mut server_as("my_users"), &ctx(), "job", Some("7")).unwrap();
    assert!(!f.sections[0].fields[0].read_only);
    // The schedule tab only for timed types; the sensor tab only for
    // sensors.
    let tab = |k: &str| f.sections.iter().find(|s| s.key == k).unwrap().when.clone();
    assert_eq!(
        tab("schedule"),
        Some(("type".into(), vec!["TASK".into(), "SENSOR".into()]))
    );
    assert_eq!(
        tab("sensor"),
        Some((
            "type".into(),
            vec!["SENSOR".into(), "STARTUP SENSOR".into()]
        ))
    );
}

#[test]
fn info_and_the_log() {
    let mut f = Fake::default().fail("sysadmin:ph_task", Some("-329"));
    let i = info(preset("informix"), &mut f, &ctx()).unwrap();
    assert!(i.kinds.is_empty() && i.notes[0].contains("sysadmin"));
    let i = info(preset("gbase8s"), &mut server(), &ctx()).unwrap();
    assert!(i
        .kinds
        .iter()
        .all(|k| k.can_create && k.can_edit && k.can_delete));
    assert!(i.notes.is_empty());
    // RESOURCE, no grants on ph_task: nothing to create, edit or delete,
    // and the forms are only read, with the reason.
    let reader = || base().rows("sysadmin:sysusers", &["usertype"], vec![vec![Some("R")]]);
    let i = info(preset("gbase8s"), &mut reader(), &ctx()).unwrap();
    assert_eq!(i.kinds.len(), 2);
    assert!(i.logs);
    assert!(i
        .notes
        .iter()
        .any(|n| n.contains("permisos sobre la base sysadmin")));
    assert!(i
        .kinds
        .iter()
        .all(|k| !k.can_create && !k.can_edit && !k.can_delete));
    for (kind, id) in [("job", Some("7")), ("group", None)] {
        let f = form(preset("informix"), &mut reader(), &ctx(), kind, id).unwrap();
        assert!(f.read_only, "{kind}");
        assert!(
            f.notes[0].contains("permisos sobre la base sysadmin"),
            "{kind}"
        );
    }
    // Granted through a role: it may write.
    let mut f = base()
        .rows("sysadmin:sysusers", &["usertype"], vec![vec![Some("C")]])
        .rows(
            "sysadmin:systabauth",
            &["tabauth"],
            vec![vec![Some("su-idx--")]],
        );
    let i = info(preset("informix"), &mut f, &ctx()).unwrap();
    assert!(i.kinds[0].can_create && i.notes.is_empty());
    // Privileges that can't be read: the server decides on the save.
    let mut f = base().fail("sysadmin:sysusers", Some("-272"));
    let i = info(preset("informix"), &mut f, &ctx()).unwrap();
    assert!(i.kinds[0].can_edit && i.notes.is_empty());
    let mut f = server()
        .rows(
            "FROM sysadmin:ph_alert a",
            &[
                "dbine_time",
                "dbine_task",
                "alert_type",
                "alert_color",
                "alert_state",
                "alert_object_type",
                "alert_object_name",
                "alert_message",
                "alert_action",
            ],
            vec![vec![
                Some("2026-11-12 03:00:00"),
                Some("mon_users"),
                Some("ERROR"),
                Some("RED"),
                Some("NEW"),
                Some("DBSPACE"),
                Some("datos"),
                Some("lleno"),
                None,
            ]],
        )
        .rows(
            "FROM sysadmin:ph_run r",
            &[
                "run_id",
                "dbine_time",
                "run_retcode",
                "run_duration",
                "dbine_task",
            ],
            vec![vec![
                Some("9"),
                Some("2026-11-12 03:00:00"),
                Some("0"),
                Some("0.2"),
                Some("mon_users"),
            ]],
        );
    let t = logs(preset("informix"), &mut f).unwrap();
    assert_eq!(t.len(), 2);
    assert_eq!(t[0].rows[0][5], "DBSPACE datos");
    assert_eq!(t[1].rows[0][1], "mon_users");
    for table in &t {
        assert_eq!(table.columns.len(), table.rows[0].len());
    }
}

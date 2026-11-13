//! Vertica: scheduled triggers and their schedules.

use super::*;
use crate::scheduler::vertica as e;

const SCHED_COLS: &[&str] = &[
    "schema_name",
    "schedule_name",
    "owner",
    "attached_trigger",
    "date_time_type",
    "date_time_string",
];

fn nightly_sched() -> Vec<Option<&'static str>> {
    vec![
        Some("public"),
        Some("nightly_sched"),
        Some("dbadmin"),
        Some("nightly_trig"),
        Some("CRON"),
        Some("0 3 * * *"),
    ]
}

fn free_sched() -> Vec<Option<&'static str>> {
    vec![
        Some("public"),
        Some("free_sched"),
        Some("dbadmin"),
        None,
        Some("DATE_TIME_LIST"),
        Some("2026-12-01 12:00:00, 2026-12-02 12:00:00"),
    ]
}

/// A superuser; trigger public.nightly_trig calls app.refresh(7,
/// 'diario') on public.nightly_sched; public.free_sched has no trigger.
fn server(superuser: bool) -> Fake {
    Fake::default()
        .rows(
            "is_super_user",
            &["S"],
            vec![vec![Some(if superuser { "t" } else { "f" })]],
        )
        .rows(
            "stored_proc_triggers WHERE",
            &[
                "schema_name",
                "trigger_name",
                "procedure_name",
                "procedure_args",
                "enabled",
                "owner",
            ],
            vec![vec![
                Some("public"),
                Some("nightly_trig"),
                Some("refresh"),
                Some("7, 'diario'"),
                Some("t"),
                Some("dbadmin"),
            ]],
        )
        .rows(
            "stored_proc_triggers ORDER BY",
            &["schema_name", "trigger_name"],
            vec![vec![Some("public"), Some("nightly_trig")]],
        )
        .rows(
            "user_procedures WHERE procedure_name = ?",
            &["schema_name", "procedure_name"],
            vec![vec![Some("app"), Some("refresh")]],
        )
        .rows(
            "user_procedures ORDER BY",
            &["schema_name", "procedure_name", "procedure_arguments"],
            vec![
                vec![Some("app"), Some("refresh"), Some("n int, kind varchar")],
                vec![Some("app"), Some("purge"), Some("")],
            ],
        )
        .rows(
            "v_catalog.schemata",
            &["schema_name"],
            vec![vec![Some("app")], vec![Some("public")]],
        )
        .rows(
            "v_catalog.users ORDER BY",
            &["user_name"],
            vec![vec![Some("dbadmin")], vec![Some("etl")]],
        )
        .rows(
            "user_schedules WHERE schema_name = ?",
            SCHED_COLS,
            vec![free_sched()],
        )
        .rows(
            "v_catalog.user_schedules",
            SCHED_COLS,
            vec![nightly_sched(), free_sched()],
        )
}

fn trigger_values(over: Value) -> FormValues {
    let f = form(
        preset("vertica"),
        &mut server(true),
        &ctx(),
        "job",
        Some("\"public\".\"nightly_trig\""),
    )
    .unwrap();
    let mut v = serde_json::to_value(&f.values).unwrap();
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({}))
}

fn edit_trigger(v: &FormValues) -> Result<String> {
    script(
        preset("vertica"),
        &mut server(true),
        "job",
        Some("\"public\".\"nightly_trig\""),
        v,
    )
}

#[test]
fn info_gives_the_kinds_to_a_superuser_only() {
    let i = info(preset("vertica"), &mut server(true), &ctx()).unwrap();
    let keys: Vec<&str> = i.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "schedule"]);
    assert!(i
        .kinds
        .iter()
        .all(|k| k.can_create && k.can_edit && k.can_delete));
    let i = info(preset("vertica"), &mut server(false), &ctx()).unwrap();
    assert!(i
        .kinds
        .iter()
        .all(|k| !k.can_create && !k.can_edit && !k.can_delete));
    assert!(i.notes.iter().any(|n| n.contains("Solo un superusuario")));
    // Not a superuser: nothing is written.
    let v = fv(
        json!({"name": "s", "schema": "public", "kind": "cron", "cron": "0 3 * * *"}),
        json!({}),
    );
    assert!(matches!(
        script(preset("vertica"), &mut server(false), "schedule", None, &v),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn a_trigger_runs_now_and_fixed_times_show_only_for_their_kind() {
    let i = info(preset("vertica"), &mut server(true), &ctx()).unwrap();
    let trig = &i.kinds[0];
    assert_eq!(trig.actions.len(), 1);
    assert_eq!(
        (trig.actions[0].key.as_str(), trig.actions[0].label.as_str()),
        ("run", "Ejecutar ahora")
    );
    // Vertica keeps no history; schedules have no actions.
    assert!(!trig.clear_history && !trig.run_from_step);
    assert!(i.kinds[1].actions.is_empty() && !i.kinds[1].clear_history);
    let id = "\"public\".\"nightly_trig\"";
    let s = action_script(
        preset("vertica"),
        &mut server(true),
        &ctx(),
        "job",
        id,
        "run",
    )
    .unwrap();
    assert!(
        s.starts_with(
            "-- Ejecuta ahora el procedimiento del trigger (EXECUTE_TRIGGER), en esta sesión"
        ) && s.contains("SELECT EXECUTE_TRIGGER('")
            && s.ends_with(");\n"),
        "{s}"
    );
    let mut f = server(true);
    action(preset("vertica"), &mut f, &ctx(), "job", id, "run").unwrap();
    assert!(f.sent("SELECT EXECUTE_TRIGGER("));
    // Not a superuser: refused before anything runs.
    let mut f = server(false);
    assert!(action(preset("vertica"), &mut f, &ctx(), "job", id, "run").is_err());
    assert!(!f.sent("EXECUTE_TRIGGER"));
    for (kind, act) in [("job", "stop"), ("schedule", "run")] {
        assert!(matches!(
            action_script(preset("vertica"), &mut server(true), &ctx(), kind, id, act),
            Err(Error::Unsupported(_))
        ));
    }
    // The fixed times are a list of the "datetimes" kind only.
    let f = form(
        preset("vertica"),
        &mut server(true),
        &ctx(),
        "schedule",
        None,
    )
    .unwrap();
    well_formed(&f);
    let times = f.sections.iter().find(|s| s.key == "times").unwrap();
    let only = Some(("kind".to_string(), vec!["datetimes".to_string()]));
    assert_eq!(times.when, only);
    assert_eq!(times.lists[0].when, only);
    assert_eq!(times.lists[0].min_rows, 1);
}

#[test]
fn schedules_are_created_with_cron_or_fixed_times() {
    let v = fv(
        json!({"name": "it's", "schema": "APP", "owner": "etl", "kind": "cron", "cron": "0 3 * * 1-5"}),
        json!({}),
    );
    assert_eq!(
        script(preset("vertica"), &mut server(true), "schedule", None, &v).unwrap(),
        "-- Programación «app.it's» de Vertica
-- ⚠ Las sentencias corren una por una, sin transacción: si una falla, las anteriores quedan hechas.
CREATE SCHEDULE \"app\".\"it's\" USING CRON '0 3 * * 1-5';
ALTER SCHEDULE \"app\".\"it's\" OWNER TO \"etl\";
"
    );
    let v = fv(
        json!({"name": "dates", "schema": "public", "kind": "datetimes"}),
        json!({"times": [{"at": "2026-12-01T12:00:00"}, {"at": "2026-12-24T23:30"}]}),
    );
    let s = script(preset("vertica"), &mut server(true), "schedule", None, &v).unwrap();
    assert!(s.contains("CREATE SCHEDULE \"public\".\"dates\" USING DATETIMES('2026-12-01 12:00:00', '2026-12-24 23:30:00');"));
    // One statement: nothing to warn about.
    assert!(warnings(&s).is_empty(), "{s}");
    let v = fv(
        json!({"name": "dates", "schema": "public", "kind": "datetimes"}),
        json!({}),
    );
    assert!(refused(script(
        preset("vertica"),
        &mut server(true),
        "schedule",
        None,
        &v
    ))
    .contains("Faltan"));
    let v = fv(
        json!({"name": "x", "schema": "nope", "kind": "cron", "cron": "0 3 * * *"}),
        json!({}),
    );
    assert!(refused(script(
        preset("vertica"),
        &mut server(true),
        "schedule",
        None,
        &v
    ))
    .contains("no existe «nope»"));
}

#[test]
fn a_schedule_changes_in_place_and_is_deleted_only_without_a_trigger() {
    let id = "\"public\".\"free_sched\"";
    let f = form(
        preset("vertica"),
        &mut server(true),
        &ctx(),
        "schedule",
        Some(id),
    )
    .unwrap();
    well_formed(&f);
    assert_eq!(val(&f, "kind"), "datetimes");
    let times = &f.sections[1].lists[0].rows;
    assert_eq!(times.len(), 2);
    let mut v = serde_json::to_value(&f.values).unwrap();
    v["kind"] = json!("cron");
    v["cron"] = json!("30 1 * * *");
    v["owner"] = json!("etl");
    v["name"] = json!("renamed");
    v["schema"] = json!("app");
    let s = script(
        preset("vertica"),
        &mut server(true),
        "schedule",
        Some(id),
        &fv(v, json!({})),
    )
    .unwrap();
    assert_eq!(
        s,
        "-- Programación «public.free_sched» de Vertica
-- ⚠ Las sentencias corren una por una, sin transacción: si una falla, las anteriores quedan hechas.
ALTER SCHEDULE \"public\".\"free_sched\" USING CRON '30 1 * * *';
ALTER SCHEDULE \"public\".\"free_sched\" OWNER TO \"etl\";
ALTER SCHEDULE \"public\".\"free_sched\" RENAME TO \"renamed\";
ALTER SCHEDULE \"public\".\"renamed\" SET SCHEMA \"app\";
"
    );
    let unchanged = fv(
        serde_json::to_value(&f.values).unwrap(),
        json!({"times": times}),
    );
    assert!(script(
        preset("vertica"),
        &mut server(true),
        "schedule",
        Some(id),
        &unchanged
    )
    .unwrap()
    .contains("no hay cambios"));
    assert_eq!(
        delete_script(preset("vertica"), &mut server(true), "schedule", id).unwrap(),
        "-- Borra la programación «public.free_sched» de Vertica\nDROP SCHEDULE \"public\".\"free_sched\";\n"
    );
    // A schedule with a trigger isn't deleted.
    let mut both = Fake::default()
        .rows("is_super_user", &["S"], vec![vec![Some("t")]])
        .rows(
            "user_schedules WHERE schema_name = ?",
            SCHED_COLS,
            vec![nightly_sched()],
        );
    assert!(refused(delete_script(
        preset("vertica"),
        &mut both,
        "schedule",
        "\"public\".\"nightly_sched\""
    ))
    .contains("borrá el trigger primero"));
}

#[test]
fn a_trigger_is_created_on_a_free_schedule() {
    let v = fv(
        json!({
            "name": "purge_trig", "schema": "app", "enabled": false, "owner": "etl",
            "procedure": "\"app\".\"purge\"", "args": " 30 , 'it''s', null ",
            "schedule": "\"public\".\"free_sched\""
        }),
        json!({}),
    );
    assert_eq!(
        script(preset("vertica"), &mut server(true), "job", None, &v).unwrap(),
        "-- Trigger programado «app.purge_trig» de Vertica
-- ⚠ El trigger se crea con tu usuario como dueño y después se le pone el suyo: si eso falla, queda tuyo y su procedimiento corre con tus privilegios.
-- ⚠ El trigger se crea habilitado y se deshabilita al final, con otra sentencia: si algo falla después de crearlo, queda habilitado y va a correr.
CREATE TRIGGER \"app\".\"purge_trig\" ON SCHEDULE \"public\".\"free_sched\" EXECUTE PROCEDURE \"app\".\"purge\"(30, 'it''s', NULL) AS DEFINER;
ALTER TRIGGER \"app\".\"purge_trig\" OWNER TO \"etl\";
SELECT ENABLE_TRIGGER('app.purge_trig', false);
"
    );
    // Enabled and owned by whoever creates it: one statement, no warning.
    let mut mine = v.clone();
    mine.values.insert("enabled".into(), json!(true));
    mine.values.insert("owner".into(), json!(null));
    let s = script(preset("vertica"), &mut server(true), "job", None, &mine).unwrap();
    assert!(
        s.contains("CREATE TRIGGER") && !s.contains("ALTER TRIGGER") && warnings(&s).is_empty(),
        "{s}"
    );
    // A schedule that already fires another trigger.
    let mut taken = v.clone();
    taken
        .values
        .insert("schedule".into(), json!("public.nightly_sched"));
    assert!(refused(script(
        preset("vertica"),
        &mut server(true),
        "job",
        None,
        &taken
    ))
    .contains("ya dispara el trigger «nightly_trig»"));
    // A procedure the server lacks.
    let mut missing = v.clone();
    missing
        .values
        .insert("procedure".into(), json!("\"app\".\"nope\""));
    assert!(refused(script(
        preset("vertica"),
        &mut server(true),
        "job",
        None,
        &missing
    ))
    .contains("«Procedimiento»: no existe"));
    let mut f = server(true);
    assert_eq!(
        save(preset("vertica"), &mut f, "job", None, &v)
            .unwrap()
            .as_deref(),
        Some("\"app\".\"purge_trig\"")
    );
    assert!(f.sent("SELECT ENABLE_TRIGGER('app.purge_trig', false)"));
}

#[test]
fn a_trigger_is_renamed_moved_and_switched_in_place() {
    let v = trigger_values(json!({"name": "nightly2", "schema": "app", "enabled": false}));
    assert_eq!(
        edit_trigger(&v).unwrap(),
        "-- Trigger programado «public.nightly_trig» de Vertica
-- ⚠ Las sentencias corren una por una, sin transacción: si una falla, las anteriores quedan hechas.
ALTER TRIGGER \"public\".\"nightly_trig\" RENAME TO \"nightly2\";
ALTER TRIGGER \"public\".\"nightly2\" SET SCHEMA \"app\";
SELECT ENABLE_TRIGGER('app.nightly2', false);
"
    );
    assert!(edit_trigger(&trigger_values(json!({})))
        .unwrap()
        .contains("no hay cambios"));
    // Only switched off: one statement, no warning.
    let s = edit_trigger(&trigger_values(json!({"enabled": false}))).unwrap();
    assert!(
        s.contains("ENABLE_TRIGGER") && warnings(&s).is_empty(),
        "{s}"
    );
}

#[test]
fn a_new_procedure_or_arguments_drop_and_create_the_trigger_with_its_owner() {
    let v = trigger_values(json!({"args": "8, 'semanal'", "enabled": false}));
    let s = edit_trigger(&v).unwrap();
    let drop = s.find("DROP TRIGGER \"public\".\"nightly_trig\";").unwrap();
    let create = s
        .find("CREATE TRIGGER \"public\".\"nightly_trig\" ON SCHEDULE \"public\".\"nightly_sched\" EXECUTE PROCEDURE \"app\".\"refresh\"(8, 'semanal') AS DEFINER;")
        .unwrap();
    let owner = s
        .find("ALTER TRIGGER \"public\".\"nightly_trig\" OWNER TO \"dbadmin\";")
        .unwrap();
    let off = s
        .find("SELECT ENABLE_TRIGGER('public.nightly_trig', false);")
        .unwrap();
    assert!(drop < create && create < owner && owner < off, "{s}");
    // Created as this login's and enabled: a failure after the creation
    // leaves it so, running as this login.
    assert_eq!(
        warnings(&s),
        [
            "Vertica no cambia el procedimiento, los argumentos ni la programación de un trigger: se borra y se crea de nuevo, y el script le vuelve a poner su dueño y su estado.",
            "Vertica confirma cada sentencia por separado: si la creación falla, el trigger queda borrado.",
            "El trigger se crea con tu usuario como dueño y después se le pone el suyo: si eso falla, queda tuyo y su procedimiento corre con tus privilegios.",
            "El trigger se crea habilitado y se deshabilita al final, con otra sentencia: si algo falla después de crearlo, queda habilitado y va a correr.",
        ],
        "{s}"
    );
    // Enabled: nothing to switch off afterwards.
    let s = edit_trigger(&trigger_values(json!({"args": "8, 'semanal'"}))).unwrap();
    assert!(!s.contains("ENABLE_TRIGGER"), "{s}");
    assert_eq!(
        warnings(&s)[2..],
        ["El trigger se crea con tu usuario como dueño y después se le pone el suyo: si eso falla, queda tuyo y su procedimiento corre con tus privilegios."],
        "{s}"
    );
    assert!(refused(edit_trigger(&trigger_values(
        json!({"args": "drop table t"})
    )))
    .contains("«Argumentos»"));
}

#[test]
fn arguments_are_literals_written_back_in_one_shape() {
    assert_eq!(e::parse_args("").unwrap(), "");
    assert_eq!(
        e::parse_args(" 1,-2.5e3 ,TRUE, null,'a,b', 'it''s' ").unwrap(),
        "1, -2.5e3, TRUE, NULL, 'a,b', 'it''s'"
    );
    for bad in ["x", "1,", "'open", "1 2", "'a' 'b'", "now()", "'a\u{0}'"] {
        assert!(e::parse_args(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_trigger_form_offers_procedures_schedules_schemas_and_users() {
    let f = form(
        preset("vertica"),
        &mut server(true),
        &ctx(),
        "job",
        Some("\"public\".\"nightly_trig\""),
    )
    .unwrap();
    well_formed(&f);
    assert!(!f.read_only);
    assert_eq!(val(&f, "procedure"), "\"app\".\"refresh\"");
    assert_eq!(val(&f, "schedule"), "\"public\".\"nightly_sched\"");
    assert_eq!(val(&f, "args"), "7, 'diario'");
    assert_eq!(f.choices["procedure"].len(), 2);
    assert!(f.choices["procedure"][0]
        .1
        .contains("refresh(n int, kind varchar)"));
    assert!(f.choices["schedule"][0]
        .1
        .contains("(trigger nightly_trig)"));
    assert_eq!(f.choices["schema"].len(), 2);
    assert_eq!(f.choices["user"].len(), 2);
    let ro = form(preset("vertica"), &mut server(false), &ctx(), "job", None).unwrap();
    well_formed(&ro);
    assert!(ro.read_only);
    let l = items(preset("vertica"), &mut server(true), &ctx(), "schedule").unwrap();
    assert_eq!(l.items.len(), 2);
    assert_eq!(l.items[0].id, "\"public\".\"free_sched\"");
    for id in ["public.nightly_trig", "\"public\".\"x\"; DROP", "\"a\""] {
        let mut f = server(true);
        assert!(
            delete_script(preset("vertica"), &mut f, "job", id).is_err(),
            "{id}"
        );
        assert!(!f.sent("stored_proc_triggers WHERE"), "{id}");
    }
}

#[test]
fn info_on_a_server_without_scheduled_procedures() {
    let mut f = Fake::default()
        .rows("v_catalog.system_tables", &["S"], vec![])
        .fail("stored_proc_triggers ORDER BY", None);
    let i = info(preset("vertica"), &mut f, &ctx()).unwrap();
    assert!(
        i.kinds.is_empty() && i.notes[0].contains("11.1.1"),
        "{:?}",
        i.notes
    );
}

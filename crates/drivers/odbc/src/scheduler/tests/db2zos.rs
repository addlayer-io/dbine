//! Db2 for z/OS's administrative task scheduler.

use super::*;
use crate::scheduler::db2zos as e;
use dbine_driver::SECRET_MASK;

const COLS: &[&str] = &[
    "TASK_NAME",
    "DESCRIPTION",
    "BEGIN_TIMESTAMP",
    "END_TIMESTAMP",
    "MAX_INVOCATIONS",
    "INTERVAL",
    "POINT_IN_TIME",
    "TRIGGER_TASK_NAME",
    "TRIGGER_TASK_COND",
    "TRIGGER_TASK_CODE",
    "DB2_SSID",
    "PROCEDURE_SCHEMA",
    "PROCEDURE_NAME",
    "PROCEDURE_INPUT",
    "JCL_LIBRARY",
    "JCL_MEMBER",
    "JOB_WAIT",
    "USERID",
    "CREATOR",
    "LAST_MODIFIED",
];

/// BACKUP calls a procedure on a cron schedule as OPS; LOADJCL submits a
/// JCL when BACKUP ends with 0.
fn server() -> Fake {
    Fake::default()
        .rows(
            "LOCAL_NOW",
            &["LOCAL_NOW", "UTC_NOW"],
            vec![vec![
                Some("2026-11-12 09:00:00.000000"),
                Some("2026-11-12 12:00:00.000000"),
            ]],
        )
        .rows(
            "DSNADM.ADMIN_TASK_LIST",
            COLS,
            vec![
                vec![
                    Some("BACKUP"),
                    Some("copia"),
                    Some("2026-01-01-00.00.00.000000"),
                    None,
                    None,
                    None,
                    Some("0 2 * * *"),
                    None,
                    None,
                    None,
                    Some("DB1A"),
                    Some("SYSPROC"),
                    Some("DSNUTILU"),
                    Some("SELECT 'X' FROM SYSIBM.SYSDUMMY1"),
                    None,
                    None,
                    None,
                    Some("OPS"),
                    Some("ADMIN"),
                    Some("2026-11-01-10.00.00.000000"),
                ],
                vec![
                    Some("LOADJCL"),
                    None,
                    None,
                    None,
                    Some("5"),
                    None,
                    None,
                    Some("BACKUP"),
                    Some("EQ"),
                    Some("0"),
                    None,
                    None,
                    None,
                    None,
                    Some("SYS1.PROCLIB"),
                    Some("LOADJ"),
                    Some("YES"),
                    None,
                    Some("ADMIN"),
                    None,
                ],
            ],
        )
        .rows(
            "DSNADM.ADMIN_TASK_STATUS",
            &["TASK_NAME", "STATUS", "NUM_INVOCATIONS", "MSG"],
            vec![vec![Some("BACKUP"), Some("COMPLETED"), Some("3"), None]],
        )
}

fn new_task(over: Value) -> FormValues {
    let mut v = json!({
        "task_name": "NIGHTLY", "description": "it's nightly", "action": "procedure",
        "procedure_schema": "SYSPROC", "procedure_name": "DSNUTILU",
        "procedure_input": "SELECT 'RUNSTATS' FROM SYSIBM.SYSDUMMY1",
        "trigger": "cron", "point_in_time": "0 3 * * *", "user_id": "OPS", "password": "s3cr'et"
    });
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({}))
}

fn backup(over: Value) -> FormValues {
    let mut v = serde_json::to_value(e::values_of(&e::task_of(
        &Set::new(
            COLS,
            vec![vec![
                Some("BACKUP"),
                Some("copia"),
                Some("2026-01-01-00.00.00.000000"),
                None,
                None,
                None,
                Some("0 2 * * *"),
                None,
                None,
                None,
                Some("DB1A"),
                Some("SYSPROC"),
                Some("DSNUTILU"),
                Some("SELECT 'X' FROM SYSIBM.SYSDUMMY1"),
                None,
                None,
                None,
                Some("OPS"),
                None,
                None,
            ]],
        ),
        0,
    )))
    .unwrap();
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({}))
}

#[test]
fn a_new_task_is_admin_task_add_with_its_password_masked_in_the_script() {
    let s = script(
        preset("db2zos"),
        &mut server(),
        "job",
        None,
        &new_task(json!({})),
    )
    .unwrap();
    assert_eq!(
        s,
        format!(
            "-- Tarea «NIGHTLY» del planificador de tareas administrativas de Db2 for z/OS
-- Los ? van como parámetros, en orden: la contraseña ('{SECRET_MASK}'); el nombre de la tarea ('NIGHTLY'), de entrada y salida: vuelve con el nombre que le dio Db2; el código de retorno y el mensaje, que devuelve el planificador.
CALL SYSPROC.ADMIN_TASK_ADD(
    'OPS',
    ?,
    NULL,
    NULL,
    NULL,
    NULL,
    '0 3 * * *',
    NULL,
    NULL,
    NULL,
    NULL,
    'SYSPROC',
    'DSNUTILU',
    'SELECT ''RUNSTATS'' FROM SYSIBM.SYSDUMMY1',
    NULL,
    NULL,
    NULL,
    ?,
    'it''s nightly',
    ?,
    ?
);
"
        )
    );
    assert!(!s.contains("s3cr"));
}

#[test]
fn the_save_binds_the_password_and_the_inout_name_and_reads_the_return_code() {
    let mut f = server().out("ADMIN_TASK_ADD", vec![Some("NIGHTLY"), Some("0"), None]);
    let id = save(preset("db2zos"), &mut f, "job", None, &new_task(json!({}))).unwrap();
    assert_eq!(id.as_deref(), Some("NIGHTLY"));
    let (sql, bound) = f
        .log
        .iter()
        .find(|(s, _)| s.contains("ADMIN_TASK_ADD"))
        .unwrap();
    // The password and the task name are markers, bound in their order;
    // neither the password nor its mask is in the statement's text.
    assert_eq!(bound, &["s3cr'et", "NIGHTLY"]);
    assert!(!sql.contains("s3cr") && !sql.contains(SECRET_MASK), "{sql}");
    assert!(
        sql.contains("CALL SYSPROC.ADMIN_TASK_ADD(\n    'OPS',\n    ?,\n"),
        "{sql}"
    );
    assert!(
        sql.ends_with("    NULL,\n    ?,\n    'it''s nightly',\n    ?,\n    ?\n)"),
        "{sql}"
    );
    // The name Db2 gives back is the id.
    let mut f = server().out("ADMIN_TASK_ADD", vec![Some(" NIGHTLY2 "), Some("0"), None]);
    let id = save(preset("db2zos"), &mut f, "job", None, &new_task(json!({}))).unwrap();
    assert_eq!(id.as_deref(), Some("NIGHTLY2"));
    // Without a user: no password marker, the name is the only value bound.
    let mut f = server();
    save(
        preset("db2zos"),
        &mut f,
        "job",
        None,
        &new_task(json!({"user_id": null, "password": null})),
    )
    .unwrap();
    let (sql, bound) = f
        .log
        .iter()
        .find(|(s, _)| s.contains("ADMIN_TASK_ADD"))
        .unwrap();
    assert_eq!(bound, &["NIGHTLY"]);
    assert!(sql.starts_with("CALL SYSPROC.ADMIN_TASK_ADD(\n    NULL,\n    NULL,\n"));
    // 12: refused, with the scheduler's message.
    let mut f = server().out(
        "ADMIN_TASK_ADD",
        vec![
            Some("NIGHTLY"),
            Some("12"),
            Some("DSNA650I THE TASK ALREADY EXISTS"),
        ],
    );
    assert_eq!(
        refused(save(
            preset("db2zos"),
            &mut f,
            "job",
            None,
            &new_task(json!({}))
        )),
        "DSNA650I THE TASK ALREADY EXISTS"
    );
}

#[test]
fn every_trigger_and_a_jcl_job_go_to_their_arguments() {
    let jcl = new_task(json!({
        "action": "jcl", "jcl_library": "sys1.proclib", "jcl_member": "loadj", "job_wait": "PURGE",
        "trigger": "task", "trigger_task_name": "BACKUP", "trigger_task_cond": "GE", "trigger_task_code": 4,
        "user_id": null, "password": null
    }));
    let (t, p) = e::from_values(&jcl).unwrap();
    assert_eq!(p, None);
    assert_eq!(t.jcl_library.as_deref(), Some("SYS1.PROCLIB"));
    assert_eq!(t.jcl_member.as_deref(), Some("LOADJ"));
    assert_eq!(t.procedure, None);
    let s = e::create_plan(&t, None, Reveal::Mask, None)
        .unwrap()
        .script();
    assert!(
        s.contains("    NULL,\n    'BACKUP',\n    'GE',\n    4,\n"),
        "{s}"
    );
    assert!(
        s.contains("'SYS1.PROCLIB',\n    'LOADJ',\n    'PURGE',"),
        "{s}"
    );
    for (trigger, want) in [("db2start", "'DB2START'"), ("db2stop", "'DB2STOP'")] {
        let (t, p) = e::from_values(&new_task(json!({"trigger": trigger}))).unwrap();
        assert!(e::create_plan(&t, p.as_deref(), Reveal::Mask, None)
            .unwrap()
            .script()
            .contains(want));
    }
    let (t, _) = e::from_values(&new_task(json!({"trigger": "interval", "interval": 15}))).unwrap();
    assert_eq!(t.interval, Some(15));
    assert_eq!(t.point, None);
    let (t, _) = e::from_values(&new_task(json!({"trigger": "once"}))).unwrap();
    assert!(t.interval.is_none() && t.point.is_none() && t.trigger_task.is_none());
}

#[test]
fn bad_values_are_refused() {
    for (over, want) in [
        (
            json!({"user_id": null}),
            "La contraseña va con un «Usuario».",
        ),
        (
            json!({"trigger": "task", "trigger_task_name": "NIGHTLY"}),
            "a sí misma",
        ),
        (
            json!({"trigger": "task", "trigger_task_name": "BACKUP", "trigger_task_cond": "EQ"}),
            "«Código»",
        ),
        (
            json!({"trigger": "task", "trigger_task_name": "BACKUP", "trigger_task_cond": "LIKE"}),
            "no es una opción válida",
        ),
        (json!({"trigger": "interval"}), "Falta «Cada (minutos)»."),
        (
            json!({"trigger": "cron", "point_in_time": null}),
            "Falta «Calendario».",
        ),
        (
            json!({"action": "jcl", "jcl_library": "BAD..LIB", "jcl_member": "M"}),
            "«Biblioteca JCL»",
        ),
        (
            json!({"action": "jcl", "jcl_library": "A.B", "jcl_member": "TOOLONGNAME"}),
            "«Miembro JCL» admite hasta 8",
        ),
        (
            json!({"action": "jcl", "jcl_library": "A.B", "jcl_member": "9X"}),
            "no es un nombre de miembro",
        ),
        (
            json!({"db2_ssid": "DB1AB"}),
            "«Miembro (data sharing)» admite hasta 4",
        ),
        (json!({"password": "a\nb"}), "«Contraseña» no es válida"),
        (json!({"password": "x".repeat(25)}), "hasta 24 caracteres"),
        (json!({"password": null}), "hace falta su «Contraseña»"),
        (json!({"password": ""}), "hace falta su «Contraseña»"),
        (json!({"action": "rexx"}), "no es una opción válida"),
    ] {
        let m = refused(
            e::from_values(&new_task(over.clone()))
                .and_then(|(t, p)| e::create_plan(&t, p.as_deref(), Reveal::Mask, None))
                .map(|p| p.script()),
        );
        assert!(m.contains(want), "{over}: {m}");
    }
}

#[test]
fn a_schedule_change_is_admin_task_update_and_a_new_name_an_add_then_a_remove() {
    // The task runs as OPS: without the password typed again, Db2 would
    // refuse any change (return code 12), so the save asks for it.
    let v = backup(json!({"trigger": "interval", "interval": 60, "point_in_time": null}));
    let m = refused(script(
        preset("db2zos"),
        &mut server(),
        "job",
        Some("BACKUP"),
        &v,
    ));
    assert!(m.contains("hace falta su «Contraseña»"), "{m}");
    let v = backup(json!({
        "trigger": "interval", "interval": 60, "point_in_time": null, "password": "pw"
    }));
    let s = script(preset("db2zos"), &mut server(), "job", Some("BACKUP"), &v).unwrap();
    assert!(
        s.contains(&format!(
            "-- Los ? van como parámetros, en orden: la contraseña ('{SECRET_MASK}'); el código de retorno"
        )),
        "{s}"
    );
    assert!(
        s.contains("-- Inicio original: 2026-01-01 00:00:00."),
        "{s}"
    );
    assert!(!s.contains("'pw'"), "{s}");
    assert!(
        s.contains(
            "CALL SYSPROC.ADMIN_TASK_UPDATE(
    'OPS',
    ?,
    NULL,
    NULL,
    NULL,
    60,
    NULL,
    NULL,
    NULL,
    NULL,
    'DB1A',
    'BACKUP',
    'copia',
    ?,
    ?
)"
        ),
        "{s}"
    );
    let mut f = server();
    save(preset("db2zos"), &mut f, "job", Some("BACKUP"), &v).unwrap();
    let (sql, bound) = f
        .log
        .iter()
        .find(|(s, _)| s.contains("ADMIN_TASK_UPDATE"))
        .unwrap();
    assert_eq!(bound, &["pw"]);
    assert!(!sql.contains("pw"), "{sql}");

    // The work can't change under the same name: removing first could
    // leave the user without the task.
    let m = refused(script(
        preset("db2zos"),
        &mut server(),
        "job",
        Some("BACKUP"),
        &backup(json!({"procedure_name": "DSNUTILV", "password": "pw"})),
    ));
    assert!(m.contains("cambiá también el «Nombre»"), "{m}");
    // With another name: the new task is added first, the old one removed
    // only after that worked.
    let f = form(
        preset("db2zos"),
        &mut server(),
        &ctx(),
        "job",
        Some("LOADJCL"),
    )
    .unwrap();
    let v = edit(&f, json!({"task_name": "LOADJCL2", "jcl_member": "LOADK"}));
    let s = script(preset("db2zos"), &mut server(), "job", Some("LOADJCL"), &v).unwrap();
    let rm = s
        .find("ADMIN_TASK_REMOVE(\n    'LOADJCL',\n    ?,\n    ?\n)")
        .unwrap();
    let add = s.find("ADMIN_TASK_ADD(").unwrap();
    assert!(add < rm && s.contains("'LOADK'"), "{s}");
    assert!(s.contains("el nombre de la tarea ('LOADJCL2')"), "{s}");
    let mut f = server().out(
        "ADMIN_TASK_ADD",
        vec![
            Some("LOADJCL2"),
            Some("12"),
            Some("DSNA679I THE TASK COULD NOT BE ADDED"),
        ],
    );
    assert_eq!(
        refused(save(preset("db2zos"), &mut f, "job", Some("LOADJCL"), &v)),
        "DSNA679I THE TASK COULD NOT BE ADDED"
    );
    assert!(
        !f.sent("ADMIN_TASK_REMOVE"),
        "a refused add removes nothing"
    );
    let mut f = server().out("ADMIN_TASK_ADD", vec![Some("LOADJCL2"), Some("0"), None]);
    let id = save(preset("db2zos"), &mut f, "job", Some("LOADJCL"), &v).unwrap();
    assert_eq!(id.as_deref(), Some("LOADJCL2"));
    assert!(f.sent("ADMIN_TASK_REMOVE"));
    // The add worked, the removal didn't: the message says both are there.
    let mut f = server()
        .out("ADMIN_TASK_ADD", vec![Some("LOADJCL2"), Some("0"), None])
        .out(
            "ADMIN_TASK_REMOVE",
            vec![Some("12"), Some("DSNA671I REMOVE FAILED")],
        );
    let m = refused(save(preset("db2zos"), &mut f, "job", Some("LOADJCL"), &v));
    assert!(
        m.contains("ahora están las dos") && m.ends_with("DSNA671I REMOVE FAILED"),
        "{m}"
    );
    assert_eq!(
        script(
            preset("db2zos"),
            &mut server(),
            "job",
            Some("BACKUP"),
            &backup(json!({}))
        )
        .unwrap(),
        "-- «BACKUP»: no hay cambios que guardar.\n"
    );
}

#[test]
fn a_task_that_triggers_others_keeps_its_name() {
    let v = backup(json!({"task_name": "BACKUP2", "password": "pw"}));
    let m = refused(script(
        preset("db2zos"),
        &mut server(),
        "job",
        Some("BACKUP"),
        &v,
    ));
    assert!(m.contains("la usan como disparador «LOADJCL»"), "{m}");
}

#[test]
fn delete_is_admin_task_remove_and_history_cant_be_cleared() {
    let mut f = server();
    assert_eq!(
        delete_script(preset("db2zos"), &mut f, "job", "LOADJCL").unwrap(),
        "-- Borra la tarea «LOADJCL» del planificador de tareas administrativas de Db2 for z/OS
CALL SYSPROC.ADMIN_TASK_REMOVE(
    'LOADJCL',
    ?,
    ?
);
"
    );
    let m = refused(delete_script(
        preset("db2zos"),
        &mut server(),
        "job",
        "NOPE",
    ));
    assert!(m.contains("no existe el job"), "{m}");
    assert!(delete_script(preset("db2zos"), &mut server(), "job", "A\nB").is_err());
}

#[test]
fn the_form_reads_the_task_and_offers_the_other_tasks_as_triggers() {
    let f = form(
        preset("db2zos"),
        &mut server(),
        &ctx(),
        "job",
        Some("LOADJCL"),
    )
    .unwrap();
    well_formed(&f);
    assert_eq!(val(&f, "action"), "jcl");
    assert_eq!(val(&f, "trigger"), "task");
    assert_eq!(val(&f, "trigger_task_name"), "BACKUP");
    assert_eq!(val(&f, "trigger_task_cond"), "EQ");
    assert_eq!(val(&f, "password"), Value::Null);
    assert_eq!(
        f.choices["task"],
        [("BACKUP".to_string(), "BACKUP".to_string())]
    );
    let f = form(
        preset("db2zos"),
        &mut server(),
        &ctx(),
        "job",
        Some("BACKUP"),
    )
    .unwrap();
    assert_eq!(val(&f, "trigger"), "cron");
    assert_eq!(val(&f, "status"), "COMPLETED");
    let new = form(preset("db2zos"), &mut server(), &ctx(), "job", None).unwrap();
    well_formed(&new);
    assert_eq!(new.choices["task"].len(), 2);
}

#[test]
fn a_running_task_is_stopped_with_admin_task_cancel() {
    let i = info(preset("db2zos"), &mut server(), &ctx()).unwrap();
    let k = &i.kinds[0];
    let keys: Vec<&str> = k.actions.iter().map(|a| a.key.as_str()).collect();
    // No run now; nothing clears a task's status.
    assert_eq!(keys, ["stop"]);
    assert_eq!(k.actions[0].label, "Detener");
    assert!(!k.clear_history && !k.run_from_step);
    assert_eq!(
        action_script(preset("db2zos"), &mut server(), &ctx(), "job", "BACKUP", "stop").unwrap(),
        "-- Detiene la ejecución en curso de la tarea «BACKUP»: ADMIN_TASK_CANCEL cancela el thread de Db2 o el job de JES en que corre.
-- Se llama en el miembro que la ejecuta (DB2_SSID de su estado). Pide MONITOR1, y para cancelar el thread o el job, autoridad para CANCEL THREAD o sobre MVS.CANCEL.JOB.
-- Los ? van como parámetros, en orden: el código de retorno y el mensaje, que devuelve el planificador.
CALL SYSPROC.ADMIN_TASK_CANCEL(
    'BACKUP',
    ?,
    ?
);
"
    );
    // The return code and message come back as output parameters: 12 is
    // the scheduler's refusal, with its message.
    let mut f = server();
    action(preset("db2zos"), &mut f, &ctx(), "job", "BACKUP", "stop").unwrap();
    assert!(f.sent("CALL SYSPROC.ADMIN_TASK_CANCEL("));
    let mut f = server().out(
        "ADMIN_TASK_CANCEL",
        vec![Some("12"), Some("DSNA691I la tarea no está corriendo")],
    );
    let m = refused(action(
        preset("db2zos"),
        &mut f,
        &ctx(),
        "job",
        "BACKUP",
        "stop",
    ));
    assert!(m.contains("no está corriendo"), "{m}");
    assert!(matches!(
        action_script(
            preset("db2zos"),
            &mut server(),
            &ctx(),
            "job",
            "BACKUP",
            "run"
        ),
        Err(Error::Unsupported(_))
    ));
    assert!(action_script(
        preset("db2zos"),
        &mut server(),
        &ctx(),
        "job",
        "NOPE",
        "stop"
    )
    .is_err());
    // A task's code goes with a return-code condition only.
    let f = form(preset("db2zos"), &mut server(), &ctx(), "job", None).unwrap();
    let code = f
        .sections
        .iter()
        .flat_map(|s| &s.fields)
        .find(|x| x.key == "trigger_task_code")
        .unwrap();
    assert_eq!(code.when, Some(("trigger".into(), vec!["task".into()])));
    assert_eq!(code.when_all.len(), 1);
    assert_eq!(code.when_all[0].0, "trigger_task_cond");
    assert!(!code.when_all[0].1.contains(&String::new()));
}

#[test]
fn another_users_task_says_who_may_change_it_and_stays_editable() {
    let who = |sqlid: &'static str, user: &'static str| {
        server().rows(
            "CURRENT SQLID",
            &["SQLID", "SESSION_USER"],
            vec![vec![Some(sqlid), Some(user)]],
        )
    };
    // BACKUP was added by ADMIN; the session is OPS.
    let f = form(
        preset("db2zos"),
        &mut who("OPS", "OPS"),
        &ctx(),
        "job",
        Some("BACKUP"),
    )
    .unwrap();
    assert!(!f.read_only);
    assert!(
        f.notes[0].starts_with("La agregó ADMIN")
            && f.notes[0].contains("SYSOPR, SYSADM o SYSCTRL"),
        "{:?}",
        f.notes
    );
    // ADMIN itself, by its SQLID or its user: no such note.
    for (sqlid, user) in [("admin", "OPS"), ("OPS", "ADMIN")] {
        let f = form(
            preset("db2zos"),
            &mut who(sqlid, user),
            &ctx(),
            "job",
            Some("BACKUP"),
        )
        .unwrap();
        assert!(
            !f.notes.iter().any(|n| n.contains("La agregó")),
            "{:?}",
            f.notes
        );
    }
    // Who the session is can't be read: nothing is assumed.
    let f = form(
        preset("db2zos"),
        &mut server(),
        &ctx(),
        "job",
        Some("BACKUP"),
    )
    .unwrap();
    assert!(
        !f.notes.iter().any(|n| n.contains("La agregó")),
        "{:?}",
        f.notes
    );
    // The tree leaves edit, delete and stop to the server.
    let j = dbine_driver::ServerJob {
        id: "BACKUP".into(),
        name: "BACKUP".into(),
        owner: Some("ADMIN".into()),
        enabled: true,
        running: true,
        can_stop: true,
        ..Default::default()
    };
    let i = job_item(preset("db2zos"), "job", j, false);
    assert_eq!((i.can_edit, i.can_delete), (None, None));
    assert_eq!(i.actions, Some(vec!["stop".to_string()]));
}

#[test]
fn info_and_logs() {
    let i = info(preset("db2zos"), &mut server(), &ctx()).unwrap();
    assert_eq!(i.kinds[0].key, "job");
    assert!(i.logs);
    let mut f = Fake::default().fail("DSNADM.ADMIN_TASK_LIST", Some("-4700"));
    let i = info(preset("db2zos"), &mut f, &ctx()).unwrap();
    assert!(i.kinds.is_empty() && i.notes[0].contains("no tiene el planificador"));
    let t = logs(preset("db2zos"), &mut server()).unwrap();
    assert_eq!(t[0].rows.len(), 1);
    assert_eq!(t[0].columns.len(), t[0].rows[0].len());
}

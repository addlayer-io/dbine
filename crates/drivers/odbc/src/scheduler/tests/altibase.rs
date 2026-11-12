//! Altibase: CREATE / ALTER / DROP JOB.

use super::*;
use crate::scheduler::altibase as e;

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

/// Job 3 `JOB1` running `app.load_data(1, 'x')` every day; the session is
/// SYS when `sys`.
fn server(sys: bool) -> Fake {
    Fake::default()
        .rows(
            "USER_NAME()",
            &["U"],
            vec![vec![Some(if sys { "SYS" } else { "APP" })]],
        )
        .rows(
            "V$PROPERTY",
            &["NAME", "VALUE1"],
            vec![
                vec![Some("JOB_SCHEDULER_ENABLE"), Some("1")],
                vec![Some("JOB_THREAD_COUNT"), Some("4")],
            ],
        )
        .rows(
            "SYS_PROCEDURES_",
            &["USER_NAME", "PROC_NAME"],
            vec![
                vec![Some("APP"), Some("LOAD_DATA")],
                vec![Some("SYS"), Some("PROC1")],
            ],
        )
        .rows_with(
            "WHERE JOB_ID = ?",
            "3",
            COLS,
            vec![vec![
                Some("3"),
                Some("JOB1"),
                Some("exec app.load_data(1, 'x')"),
                Some("2026-11-12 03:00:00"),
                Some("2026-12-31 00:00:00"),
                Some("1"),
                Some("DD"),
                Some("0"),
                Some("2026-11-11 03:00:00"),
                Some("5"),
                None,
                Some("T"),
                Some("carga"),
            ]],
        )
}

fn form_of() -> ObjectForm {
    form(
        preset("altibase"),
        &mut server(true),
        &ctx(),
        "job",
        Some("3"),
    )
    .unwrap()
}

fn new_job(over: Value) -> FormValues {
    let mut v = json!({
        "name": "nightly_load", "procedure": "APP.LOAD_DATA", "args": "1, 'it''s'",
        "start": "2026-11-20T03:00:00", "interval": 1, "unit": "DAY", "enabled": true,
        "comment": "carga diaria"
    });
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({}))
}

#[test]
fn a_new_job_is_create_job_with_every_clause() {
    assert_eq!(
        script(
            preset("altibase"),
            &mut server(true),
            "job",
            None,
            &new_job(json!({}))
        )
        .unwrap(),
        "-- Job «nightly_load» de Altibase
CREATE JOB nightly_load
EXEC APP.LOAD_DATA(1, 'it''s')
START TO_DATE('2026-11-20 03:00:00', 'YYYY-MM-DD HH24:MI:SS')
INTERVAL 1 DAY
ENABLE
COMMENT 'carga diaria';
"
    );
    let s = script(
        preset("altibase"),
        &mut server(true),
        "job",
        None,
        &new_job(json!({"name": "My Job", "start": null, "interval": null, "end": "2027-01-01T00:00:00", "args": null, "enabled": false, "comment": null})),
    )
    .unwrap();
    assert_eq!(
        s,
        "-- Job «My Job» de Altibase
CREATE JOB \"My Job\"
EXEC APP.LOAD_DATA
START SYSDATE
END TO_DATE('2027-01-01 00:00:00', 'YYYY-MM-DD HH24:MI:SS')
DISABLE;
"
    );
    let mut f = server(true).rows("WHERE JOB_NAME = ?", &["JOB_ID"], vec![vec![Some("8")]]);
    let id = save(preset("altibase"), &mut f, "job", None, &new_job(json!({}))).unwrap();
    assert_eq!(id.as_deref(), Some("8"));
    assert_eq!(f.log.last().unwrap().1, ["NIGHTLY_LOAD"]);
}

#[test]
fn an_edit_is_one_alter_job_per_change() {
    let f = form_of();
    well_formed(&f);
    assert_eq!(val(&f, "procedure"), "APP.LOAD_DATA");
    assert_eq!(val(&f, "args"), "1, 'x'");
    assert_eq!(val(&f, "interval"), 1);
    assert_eq!(val(&f, "unit"), "DAY");
    assert_eq!(val(&f, "exec_count"), "5");
    assert_eq!(val(&f, "running"), "No");
    assert!(!f.read_only);
    assert_eq!(
        script(
            preset("altibase"),
            &mut server(true),
            "job",
            Some("3"),
            &edit(&f, json!({}))
        )
        .unwrap(),
        "-- «JOB1»: no hay cambios que guardar.\n"
    );
    // The same name in small letters is the same job.
    assert!(script(
        preset("altibase"),
        &mut server(true),
        "job",
        Some("3"),
        &edit(&f, json!({"name": "job1"}))
    )
    .unwrap()
    .contains("no hay cambios"));
    let v = edit(
        &f,
        json!({"args": "2, 'x'", "interval": 2, "unit": "HOUR", "enabled": false, "comment": null,
               "start": "2026-11-13T04:00:00"}),
    );
    assert_eq!(
        script(preset("altibase"), &mut server(true), "job", Some("3"), &v).unwrap(),
        "-- Job «JOB1» de Altibase
ALTER JOB \"JOB1\" SET EXEC APP.LOAD_DATA(2, 'x');
ALTER JOB \"JOB1\" SET START TO_DATE('2026-11-13 04:00:00', 'YYYY-MM-DD HH24:MI:SS');
ALTER JOB \"JOB1\" SET INTERVAL 2 HOUR;
ALTER JOB \"JOB1\" SET DISABLE;
ALTER JOB \"JOB1\" SET COMMENT '';
"
    );
}

#[test]
fn a_new_name_or_taking_the_end_away_creates_the_job_again() {
    let f = form_of();
    for over in [
        json!({"name": "JOB2"}),
        json!({"end": null}),
        json!({"interval": null}),
    ] {
        let s = script(
            preset("altibase"),
            &mut server(true),
            "job",
            Some("3"),
            &edit(&f, over.clone()),
        )
        .unwrap();
        let drop = s.find("DROP JOB \"JOB1\";").unwrap();
        let create = s.find("CREATE JOB ").unwrap();
        assert!(drop < create, "{over}: {s}");
        assert!(
            s.contains("-- Altibase no cambia el nombre de un job"),
            "{s}"
        );
        assert!(s.contains("START TO_DATE('2026-11-12 03:00:00'"), "{s}");
    }
    let m = refused(script(
        preset("altibase"),
        &mut server(true),
        "job",
        Some("3"),
        &edit(&f, json!({"start": null})),
    ));
    assert!(m.contains("sin inicio"), "{m}");
    assert_eq!(
        delete_script(preset("altibase"), &mut server(true), "job", "3").unwrap(),
        "-- Borra el job «JOB1» de Altibase\nDROP JOB \"JOB1\";\n"
    );
}

#[test]
fn only_sys_changes_jobs() {
    let i = info(preset("altibase"), &mut server(false), &ctx()).unwrap();
    assert!(!i.kinds[0].can_create && !i.kinds[0].can_edit && !i.kinds[0].can_delete);
    assert!(i.notes[0].contains("SYS"));
    let f = form(
        preset("altibase"),
        &mut server(false),
        &ctx(),
        "job",
        Some("3"),
    )
    .unwrap();
    assert!(f.read_only);
    assert!(matches!(
        script(
            preset("altibase"),
            &mut server(false),
            "job",
            None,
            &new_job(json!({}))
        ),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        delete_script(preset("altibase"), &mut server(false), "job", "3"),
        Err(Error::Unsupported(_))
    ));
    let i = info(preset("altibase"), &mut server(true), &ctx()).unwrap();
    assert!(i.kinds[0].can_create && i.notes.is_empty());
}

#[test]
fn before_6_5_1_there_is_no_enable_nor_comment() {
    let mut f = server(true).fail(
        "IS_ENABLE, \"COMMENT\" FROM SYSTEM_.SYS_JOBS_ WHERE JOB_ID = -1",
        None,
    );
    let s = script(preset("altibase"), &mut f, "job", None, &new_job(json!({}))).unwrap();
    assert!(!s.contains("ENABLE") && !s.contains("COMMENT"), "{s}");
    let mut f = server(true).fail(
        "IS_ENABLE, \"COMMENT\" FROM SYSTEM_.SYS_JOBS_ WHERE JOB_ID = -1",
        None,
    );
    let i = info(preset("altibase"), &mut f, &ctx()).unwrap();
    assert!(i.notes.iter().any(|n| n.contains("6.5.1")));
}

#[test]
fn the_call_and_the_names_are_checked() {
    for (over, want) in [
        (json!({"args": "1); DROP TABLE t; --"}), "«Argumentos»"),
        (json!({"args": "'open"}), "«Argumentos»"),
        (json!({"args": "1 /* x */"}), "«Argumentos»"),
        (json!({"procedure": "x; drop"}), "«Procedimiento»"),
        (json!({"end": "2026-11-19T00:00:00"}), "posterior"),
        (json!({"unit": "WEEK"}), "«Unidad»"),
        (json!({"interval": 0}), "«Cada»"),
        (json!({"name": ""}), "Falta «Nombre»."),
    ] {
        let m = refused(e::from_values(&new_job(over.clone()), &[]));
        assert!(m.contains(want), "{over}: {m}");
    }
    assert!(e::args_ok("f('a;b', (1 + 2)), 'it''s'"));
    assert!(e::proc_name("\"app\".\"load\"") && e::proc_name("proc1") && !e::proc_name("a.b.c"));
    assert_eq!(
        e::split_call("app.p(1, 'a(b')"),
        ("app.p".into(), Some("1, 'a(b'".into()))
    );
    assert_eq!(e::split_call("p"), ("p".into(), None));
    assert_eq!(e::norm_proc("proc1"), "SYS.PROC1");
    assert_eq!(e::norm_proc("app.load"), "APP.LOAD");
    assert_eq!(e::norm_proc("\"APP\".\"LOAD\""), "APP.LOAD");
    assert!(matches!(
        clear_history(preset("altibase"), &mut server(true), "3"),
        Err(Error::Unsupported(_))
    ));
}

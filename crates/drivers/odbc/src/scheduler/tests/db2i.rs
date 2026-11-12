//! Db2 for i: the job scheduler's entries.

use super::*;
use crate::scheduler::db2i as e;

const COLS: &[&str] = &[
    "SCHEDULED_JOB_ENTRY_NUMBER",
    "SCHEDULED_JOB_NAME",
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
    "JOB_DESCRIPTION_NAME",
    "JOB_DESCRIPTION_LIBRARY_NAME",
    "USER_PROFILE_FOR_SUBMITTED_JOB",
    "MESSAGE_QUEUE_NAME",
    "MESSAGE_QUEUE_LIBRARY_NAME",
    "KEEP_ENTRY",
    "COMMAND_STRING",
];

/// CLEANUP (123): weekly on Monday and Wednesday at 03:00, held.
fn cleanup(n: &'static str) -> Vec<Option<&'static str>> {
    vec![
        Some(n),
        Some("CLEANUP"),
        Some("HELD"),
        Some("limpieza"),
        Some("ANA"),
        Some("*WEEKLY"),
        Some("*NONE"),
        None,
        Some("03.00.00"),
        Some("*MON *WED"),
        None,
        Some("2026-11-16"),
        Some("2026-12-25 2027-01-01"),
        Some("*SBMRLS"),
        Some("QBATCH"),
        Some("QGPL"),
        Some("*USRPRF"),
        None,
        Some("ANA"),
        Some("*USRPRF"),
        None,
        Some("NO"),
        Some("CALL PGM(APP/LIMPIA)"),
    ]
}

fn server() -> Fake {
    Fake::default().rows("QSYS2.SCHEDULED_JOB_INFO", COLS, vec![cleanup("123")])
}

fn new_entry(over: Value) -> FormValues {
    let mut v = json!({
        "name": "nightly", "text": "it's", "command": "CALL PGM(LIB/P) PARM('x')",
        "enabled": true, "frequency": "*WEEKLY", "date_mode": "*NONE", "days": ["*MON", "*FRI"],
        "time": "03:30", "recovery": "*SBMRLS", "jobd": "*USRPRF", "jobq": "*JOBD",
        "user": "*CURRENT", "msgq": "*USRPRF"
    });
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({}))
}

fn cleanup_values(over: Value, omit: Value) -> FormValues {
    let f = form(
        preset("db2i"),
        &mut server(),
        &ctx(),
        "job",
        Some("000123/CLEANUP"),
    )
    .unwrap();
    let mut v = serde_json::to_value(&f.values).unwrap();
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({ "omit": omit }))
}

fn kept_omit() -> Value {
    json!([{"date": "2026-12-25"}, {"date": "2027-01-01"}])
}

#[test]
fn a_new_entry_is_addjobscde_through_qcmdexc_with_its_quotes_doubled_twice() {
    let s = script(
        preset("db2i"),
        &mut server(),
        "job",
        None,
        &new_entry(json!({})),
    )
    .unwrap();
    assert_eq!(
        s,
        "-- Entrada «NIGHTLY» del planificador de trabajos de IBM i
CALL QSYS2.QCMDEXC('ADDJOBSCDE JOB(NIGHTLY) CMD(CALL PGM(LIB/P) PARM(''x'')) FRQ(*WEEKLY) SCDDATE(*NONE) SCDDAY(*MON *FRI) SCDTIME(033000) RCYACN(*SBMRLS) JOBD(*USRPRF) JOBQ(*JOBD) USER(*CURRENT) MSGQ(*USRPRF) TEXT(''it''''s'')');
"
    );
}

#[test]
fn dates_go_as_yymmdd_after_the_date_format_and_once_entries_say_if_they_stay() {
    let v = fv(
        json!({
            "name": "ONCE1", "command": "DSPLIB QGPL", "frequency": "*ONCE", "date_mode": "date",
            "date": "2026-12-01", "time": "23:59:59", "keep": true, "jobd": "app/nightjd",
            "jobq": "QBATCH", "user": "ops", "msgq": "*NONE", "recovery": "*NOSBM"
        }),
        json!({"omit": [{"date": "2026-12-24"}, {"date": "2026-12-24"}, {"date": null}]}),
    );
    let s = script(preset("db2i"), &mut server(), "job", None, &v).unwrap();
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[1], "CALL QSYS2.QCMDEXC('CHGJOB DATFMT(*YMD)');");
    assert_eq!(
        lines[2],
        "CALL QSYS2.QCMDEXC('ADDJOBSCDE JOB(ONCE1) CMD(DSPLIB QGPL) FRQ(*ONCE) SCDDATE(261201) SCDTIME(235959) SAVE(*YES) OMITDATE(261224) RCYACN(*NOSBM) JOBD(APP/NIGHTJD) JOBQ(QBATCH) USER(OPS) MSGQ(*NONE) TEXT(*BLANK)');"
    );
}

#[test]
fn a_held_entry_is_held_after_it_is_added() {
    let s = script(
        preset("db2i"),
        &mut server(),
        "job",
        None,
        &new_entry(json!({"enabled": false})),
    )
    .unwrap();
    assert!(
        s.ends_with("CALL QSYS2.QCMDEXC('HLDJOBSCDE JOB(NIGHTLY) ENTRYNBR(*ONLY)');\n"),
        "{s}"
    );
    // Another CLEANUP exists: the new one is held by its number, found after.
    let (e1, all) = (
        e::from_values(&new_entry(json!({"name": "CLEANUP", "enabled": false}))).unwrap(),
        Set::new(COLS, vec![cleanup("123")]),
    );
    let p = e::create_plan(&e1, &all);
    let s = p.script();
    assert!(!s.contains("QCMDEXC('HLDJOBSCDE"), "{s}");
    assert!(s.contains("-- Hay otras entradas «CLEANUP»"), "{s}");
    let mut f = Fake::default().rows(
        "QSYS2.SCHEDULED_JOB_INFO",
        COLS,
        vec![cleanup("123"), cleanup("300")],
    );
    assert_eq!(
        execute(&mut f, p).unwrap().as_deref(),
        Some("000300/CLEANUP")
    );
    assert!(f.sent("HLDJOBSCDE JOB(CLEANUP) ENTRYNBR(000300)"));
    // The hold fails (no authority) or the number isn't found: the save
    // fails and says the entry was left released.
    let mut f = Fake::default()
        .rows(
            "QSYS2.SCHEDULED_JOB_INFO",
            COLS,
            vec![cleanup("123"), cleanup("300")],
        )
        .fail("HLDJOBSCDE", Some("CPF1637"));
    let m = refused(execute(&mut f, e::create_plan(&e1, &all)));
    assert!(
        m.contains("La entrada 000300 se agregó pero HLDJOBSCDE falló: quedó liberada"),
        "{m}"
    );
    let mut f = Fake::default().rows("QSYS2.SCHEDULED_JOB_INFO", COLS, vec![cleanup("123")]);
    let m = refused(execute(&mut f, e::create_plan(&e1, &all)));
    assert!(m.contains("no se encontró su número"), "{m}");
    // An entry that needn't be held keeps the save when its id isn't found.
    let e2 = e::from_values(&new_entry(json!({"name": "CLEANUP"}))).unwrap();
    let mut f = Fake::default().fail("QSYS2.SCHEDULED_JOB_INFO", None);
    assert!(execute(&mut f, e::create_plan(&e2, &all)).is_ok());
}

#[test]
fn an_edit_is_chgjobscde_with_only_what_changed() {
    let v = cleanup_values(json!({"text": "otra"}), kept_omit());
    let s = script(
        preset("db2i"),
        &mut server(),
        "job",
        Some("000123/CLEANUP"),
        &v,
    )
    .unwrap();
    assert_eq!(
        s,
        "-- Entrada «CLEANUP» del planificador de trabajos de IBM i
CALL QSYS2.QCMDEXC('CHGJOBSCDE JOB(CLEANUP) ENTRYNBR(000123) TEXT(''otra'')');
"
    );
    // The schedule's parameters go together; days cleared for a date.
    let v = cleanup_values(
        json!({"frequency": "*MONTHLY", "date_mode": "*MONTHEND", "days": []}),
        kept_omit(),
    );
    let s = script(
        preset("db2i"),
        &mut server(),
        "job",
        Some("000123/CLEANUP"),
        &v,
    )
    .unwrap();
    assert!(
        s.contains("CHGJOBSCDE JOB(CLEANUP) ENTRYNBR(000123) FRQ(*MONTHLY) SCDDATE(*MONTHEND) SCDDAY(*NONE) SCDTIME(030000) RELDAYMON(*NONE)"),
        "{s}"
    );
    // Relative days of a monthly entry.
    let v = cleanup_values(
        json!({"frequency": "*MONTHLY", "relative": ["1", "*LAST"]}),
        kept_omit(),
    );
    let s = script(
        preset("db2i"),
        &mut server(),
        "job",
        Some("000123/CLEANUP"),
        &v,
    )
    .unwrap();
    assert!(
        s.contains("SCDDAY(*MON *WED) SCDTIME(030000) RELDAYMON(1 *LAST)"),
        "{s}"
    );
    // Released, and the omitted dates cleared.
    let v = cleanup_values(json!({"enabled": true}), json!([]));
    let s = script(
        preset("db2i"),
        &mut server(),
        "job",
        Some("000123/CLEANUP"),
        &v,
    )
    .unwrap();
    assert!(
        s.contains("CHGJOBSCDE JOB(CLEANUP) ENTRYNBR(000123) OMITDATE(*NONE)"),
        "{s}"
    );
    assert!(
        s.contains("RLSJOBSCDE JOB(CLEANUP) ENTRYNBR(000123)"),
        "{s}"
    );
    // Nothing changed.
    let v = cleanup_values(json!({}), kept_omit());
    assert_eq!(
        script(
            preset("db2i"),
            &mut server(),
            "job",
            Some("000123/CLEANUP"),
            &v
        )
        .unwrap(),
        "-- «CLEANUP»: no hay cambios que guardar.\n"
    );
}

#[test]
fn a_new_name_adds_the_entry_again_and_then_removes_the_old_one() {
    let v = cleanup_values(json!({"name": "CLEAN2"}), kept_omit());
    let s = script(
        preset("db2i"),
        &mut server(),
        "job",
        Some("000123/CLEANUP"),
        &v,
    )
    .unwrap();
    let rm = s.find("RMVJOBSCDE JOB(CLEANUP) ENTRYNBR(000123)").unwrap();
    let add = s.find("ADDJOBSCDE JOB(CLEAN2)").unwrap();
    let hold = s.find("HLDJOBSCDE JOB(CLEAN2) ENTRYNBR(*ONLY)").unwrap();
    // A refused add leaves the old entry as it was.
    assert!(add < hold && hold < rm, "{s}");
    assert!(
        s.contains("-- IBM i no cambia el nombre de una entrada"),
        "{s}"
    );
}

#[test]
fn a_command_cant_reach_past_its_parameter() {
    e::cl_command("CALL PGM(A/B) PARM('a)b' 'x''y')").unwrap();
    for bad in [
        "DSPLIB) USER(QSECOFR",
        "CALL PGM(A/B",
        "SNDMSG MSG('hola) TOUSR(X)",
        "DSPLIB\nQGPL",
    ] {
        assert!(e::cl_command(bad).is_err(), "{bad}");
    }
    assert!(e::cl_command(&"X".repeat(513)).is_err());
}

#[test]
fn bad_values_are_refused() {
    for (over, want) in [
        (json!({"name": "9LIVES"}), "no es un nombre de IBM i"),
        (json!({"name": "TOOLONGNAME"}), "admite hasta 10 caracteres"),
        (json!({"days": []}), "necesita los «Días»"),
        (
            json!({"days": ["*ALL", "*MON"]}),
            "«Todos» los días va solo.",
        ),
        (json!({"days": ["*XYZ"]}), "no es un día de la semana"),
        (
            json!({"date_mode": "date", "date": "2040-01-01"}),
            "entre 1940 y 2039",
        ),
        (json!({"date_mode": "date"}), "Falta «Fecha de envío»."),
        (json!({"frequency": "*DAILY"}), "no es una opción válida"),
        (json!({"jobd": "*LIBL"}), "«Descripción de trabajo»"),
        (json!({"jobq": "A/B/C"}), "«Cola de trabajos»"),
        (json!({"user": "*ALL"}), "«Usuario»"),
        (json!({"msgq": "LIB/"}), "«Cola de mensajes»"),
        (
            json!({"text": "x".repeat(51)}),
            "«Descripción» admite hasta 50",
        ),
        (json!({"time": "25:00"}), "«Hora»"),
    ] {
        let m = refused(e::from_values(&new_entry(over.clone())));
        assert!(m.contains(want), "{over}: {m}");
    }
    let too_many: Vec<Value> = (1..=21)
        .map(|d| json!({"date": format!("2026-12-{d:02}")}))
        .collect();
    let v = fv(
        serde_json::to_value(new_entry(json!({})).values).unwrap(),
        json!({ "omit": too_many }),
    );
    assert!(refused(e::from_values(&v)).contains("hasta 20"));
}

#[test]
fn delete_is_rmvjobscde_of_that_entry_and_forged_ids_are_refused() {
    assert_eq!(
        delete_script(preset("db2i"), &mut server(), "job", "123/cleanup").unwrap(),
        "-- Borra la entrada «CLEANUP» (000123) del planificador de trabajos de IBM i
CALL QSYS2.QCMDEXC('RMVJOBSCDE JOB(CLEANUP) ENTRYNBR(000123)');
"
    );
    for id in [
        "000123/CLEANUP) USER(QSECOFR",
        "1234567/X",
        "x/CLEANUP",
        "000123",
    ] {
        let mut f = server();
        assert!(
            delete_script(preset("db2i"), &mut f, "job", id).is_err(),
            "{id}"
        );
        assert!(f.log.is_empty(), "{id}");
    }
}

#[test]
fn the_form_reads_the_entry() {
    let f = form(
        preset("db2i"),
        &mut server(),
        &ctx(),
        "job",
        Some("000123/CLEANUP"),
    )
    .unwrap();
    well_formed(&f);
    assert_eq!(val(&f, "enabled"), false);
    assert_eq!(val(&f, "date_mode"), "*NONE");
    assert_eq!(val(&f, "days"), json!(["*MON", "*WED"]));
    assert_eq!(val(&f, "time"), "03:00:00");
    assert_eq!(val(&f, "jobq"), "QGPL/QBATCH");
    assert_eq!(val(&f, "entry"), "000123");
    let omit = &f.sections[1].lists[0].rows;
    assert_eq!(omit.len(), 2);
    assert_eq!(omit[0]["date"], "2026-12-25");
    let new = form(preset("db2i"), &mut server(), &ctx(), "job", None).unwrap();
    well_formed(&new);
    assert_eq!(val(&new, "user"), "*CURRENT");
}

#[test]
fn a_save_answers_the_new_entrys_id() {
    let e1 = e::from_values(&new_entry(json!({}))).unwrap();
    let p = e::create_plan(&e1, &Set::new(COLS, vec![cleanup("123")]));
    let mut row = cleanup("200");
    row[1] = Some("NIGHTLY");
    let mut f = Fake::default().rows("QSYS2.SCHEDULED_JOB_INFO", COLS, vec![cleanup("123"), row]);
    assert_eq!(
        execute(&mut f, p).unwrap().as_deref(),
        Some("000200/NIGHTLY")
    );
    assert!(f.tx.is_empty() || f.tx == ["commit"]);
}

#[test]
fn info_says_what_the_view_or_authority_lacks() {
    let mut f = Fake::default().fail("QSYS2.SCHEDULED_JOB_INFO", Some("-204"));
    let i = info(preset("db2i"), &mut f, &ctx()).unwrap();
    assert!(i.kinds.is_empty() && i.notes[0].contains("SCHEDULED_JOB_INFO"));
    let i = info(preset("db2i"), &mut server(), &ctx()).unwrap();
    assert_eq!(i.kinds[0].key, "job");
    assert!(i.notes.iter().any(|n| n.contains("*JOBCTL")));
}

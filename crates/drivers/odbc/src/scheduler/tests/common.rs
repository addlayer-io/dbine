//! What every engine shares: the values' checks and how a plan runs.

use super::*;

#[test]
fn the_engines_of_this_part_are_supported_and_the_rest_not() {
    for id in [
        "db2", "db2zos", "db2i", "informix", "gbase8s", "hive", "cloudera", "vertica",
    ] {
        assert!(supported(preset(id)), "{id}");
        assert!(crate::design::capabilities(preset(id)).scheduler, "{id}");
    }
    for id in ["odbc", "teradata", "exasol", "netezza", "impala"] {
        assert!(!supported(preset(id)), "{id}");
        assert!(!crate::design::capabilities(preset(id)).scheduler, "{id}");
        let mut f = Fake::default();
        assert!(matches!(
            info(preset(id), &mut f, &ctx()),
            Err(Error::Unsupported(_))
        ));
        assert!(f.log.is_empty());
    }
}

#[test]
fn no_engine_here_starts_a_job_at_a_step() {
    for id in ["db2", "db2zos", "db2i", "informix", "hive", "vertica"] {
        assert!(matches!(
            run_from(preset(id), "1", "x"),
            Err(Error::Unsupported(m)) if m.contains("un solo paso")
        ));
    }
}

#[test]
fn each_kind_offers_what_its_engine_documents() {
    // (preset, kind, actions, clear history); every other kind of these
    // engines offers nothing.
    let table: &[(&str, &str, &[&str], bool)] = &[
        ("db2", "job", &[], true),
        ("db2zos", "job", &["stop"], false),
        ("db2i", "job", &[], false),
        ("informix", "job", &["run"], true),
        ("gbase8s", "job", &["run"], true),
        ("informix", "group", &[], false),
        ("hive", "job", &["run", "stop"], false),
        ("cloudera", "job", &["run", "stop"], false),
        ("vertica", "job", &["run"], false),
        ("vertica", "schedule", &[], false),
        ("sybase", "job", &["run", "stop"], true),
        ("sybase", "jobdef", &[], false),
        ("sybase", "schedule", &[], false),
        ("sqlanywhere", "job", &["run"], false),
        ("iris", "job", &[], false),
        ("cache", "job", &[], false),
        ("dameng", "job", &["run", "stop"], true),
        ("dameng", "dbms_job", &["run", "stop"], true),
        ("dameng", "scheduler_job", &["run", "stop"], true),
        ("dameng", "operator", &[], false),
        ("dameng", "alert", &[], false),
        ("altibase", "job", &[], false),
        ("virtuoso", "job", &[], false),
    ];
    for (id, kind, actions, clear) in table {
        let o = offers(preset(id), kind);
        assert_eq!(o.actions, *actions, "{id} {kind}");
        assert_eq!(o.clear_history, *clear, "{id} {kind}");
    }
    // An engine without actions refuses any, before the server.
    for (id, kind) in [
        ("db2", "job"),
        ("db2i", "job"),
        ("iris", "job"),
        ("altibase", "job"),
        ("virtuoso", "job"),
    ] {
        for act in ["run", "stop"] {
            let mut f = Fake::default();
            assert!(
                matches!(
                    action_script(preset(id), &mut f, &ctx(), kind, "1", act),
                    Err(Error::Unsupported(_))
                ),
                "{id} {act}"
            );
            assert!(f.log.is_empty(), "{id} {act}");
        }
    }
    // A kind the engine lacks, before the server too.
    let mut f = Fake::default();
    assert!(refused(action_script(
        preset("hive"),
        &mut f,
        &ctx(),
        "group",
        "1",
        "run"
    ))
    .contains("no es un tipo de objeto"));
    assert!(f.log.is_empty());
}

#[test]
fn an_item_offers_what_it_can_now_and_hides_what_cant_change() {
    let job = |name: &str| dbine_driver::ServerJob {
        id: "1".into(),
        name: name.into(),
        enabled: true,
        ..Default::default()
    };
    // Run where the list says it runs; stop while it runs.
    let mut j = job("a");
    j.can_run = true;
    let i = job_item(preset("sybase"), "job", j, true);
    assert_eq!(i.actions, Some(vec!["run".to_string()]));
    assert_eq!((i.can_edit, i.can_delete), (None, None));
    let mut j = job("a");
    j.can_stop = true;
    j.running = true;
    let i = job_item(preset("sybase"), "job", j, true);
    assert_eq!(i.actions, Some(vec!["stop".to_string()]));
    // z/OS: stop while it runs, as the jobs list says (both go through
    // ADMIN_TASK_CANCEL's plan).
    let mut j = job("BACKUP");
    j.running = true;
    j.can_stop = true;
    let i = job_item(preset("db2zos"), "job", j, false);
    assert_eq!(i.actions, Some(vec!["stop".to_string()]));
    assert_eq!(i.enabled, None);
    let i = job_item(preset("db2zos"), "job", job("BACKUP"), false);
    assert_eq!(i.actions, Some(Vec::new()));
    // Without actions in the kind: nothing to say.
    assert_eq!(
        job_item(preset("db2"), "job", job("t"), false).actions,
        None
    );
    // Informix's built-in tasks aren't deleted (they're tuned); IBM i's
    // IBM-supplied entries are only read; a Hive query of another
    // namespace and another user's Dameng job too.
    let mut j = job("mon_users");
    j.system = true;
    let i = job_item(preset("informix"), "job", j, true);
    assert!(i.system);
    assert_eq!((i.can_edit, i.can_delete), (None, Some(false)));
    let mut j = job("QEZBKTMSUN");
    j.system = true;
    let i = job_item(preset("db2i"), "job", j, true);
    assert_eq!((i.can_edit, i.can_delete), (Some(false), Some(false)));
    for p in ["hive", "dameng"] {
        let i = job_item(preset(p), "job", job("x"), true);
        assert_eq!(
            (i.can_edit, i.can_delete),
            (Some(false), Some(false)),
            "{p}"
        );
        let mut j = job("x");
        j.can_enable = true;
        let i = job_item(preset(p), "job", j, true);
        assert_eq!((i.can_edit, i.can_delete), (None, None), "{p}");
    }
}

#[test]
fn a_kind_the_engine_lacks_is_refused_before_the_server() {
    for (id, kind) in [
        ("db2", "schedule"),
        ("db2zos", "group"),
        ("db2i", "operator"),
        ("informix", "schedule"),
        ("hive", "group"),
        ("vertica", "group"),
        ("vertica", "x'; DROP TABLE t --"),
    ] {
        let mut f = Fake::default();
        let m = refused(form(preset(id), &mut f, &ctx(), kind, None));
        assert!(m.contains("no es un tipo de objeto"), "{id}: {m}");
        assert!(f.log.is_empty(), "{id}");
    }
}

#[test]
fn values_read_whatever_json_type_they_come_as() {
    let m = map(json!({
        "t": "  a b ", "n": 5, "nf": 5.0, "ns": " 7 ", "frac": 2.5, "b1": true, "b2": "true",
        "b3": 1, "b4": "no", "list": ["x", " y ", "", null, 3], "one": "z, w", "null": null
    }));
    let v = Vals(&m);
    assert_eq!(v.raw("t"), "  a b ");
    assert_eq!(v.text("t"), "a b");
    assert_eq!(v.opt("null"), None);
    assert_eq!(v.opt("missing"), None);
    assert_eq!(v.int("n", "N", 0, 10).unwrap(), Some(5));
    assert_eq!(v.int("nf", "N", 0, 10).unwrap(), Some(5));
    assert_eq!(v.int("ns", "N", 0, 10).unwrap(), Some(7));
    assert_eq!(v.int("missing", "N", 0, 10).unwrap(), None);
    assert!(v.int("frac", "N", 0, 10).is_err());
    assert_eq!(
        refused(v.int("n", "Reintentos", 6, 10)),
        "«Reintentos» tiene que estar entre 6 y 10 (es 5)."
    );
    assert!(v.bool_or("b1", false) && v.bool_or("b2", false) && v.bool_or("b3", false));
    assert!(!v.bool_or("b4", true));
    assert!(v.bool_or("missing", true) && !v.bool_or("null", false));
    assert_eq!(v.strings("list"), ["x", "y", "3"]);
    assert_eq!(v.strings("one"), ["z", "w"]);
}

#[test]
fn texts_are_checked_for_length_and_control_characters() {
    let m = map(
        json!({"a": "ok", "b": "x\ny", "c": "ñandú", "d": "  ", "e": "a\u{0}b", "f": "SELECT 1\n\tFROM t;\n"}),
    );
    let v = Vals(&m);
    assert_eq!(required(v, "a", "A", 2).unwrap(), "ok");
    assert!(refused(required(v, "a", "A", 1)).contains("hasta 1 caracteres"));
    assert!(refused(required(v, "b", "B", 10)).contains("saltos de línea"));
    assert_eq!(required(v, "c", "C", 5).unwrap(), "ñandú");
    assert_eq!(refused(required(v, "d", "D", 5)), "Falta «D».");
    assert_eq!(optional(v, "d", "D", 5).unwrap(), None);
    assert!(refused(code_text(v, "e", "E", 100)).contains("control"));
    assert_eq!(
        code_text(v, "f", "F", 100).unwrap().as_deref(),
        Some("SELECT 1\n\tFROM t;")
    );
    assert!(refused(code_text(v, "f", "F", 5)).contains("hasta 5 bytes"));
}

#[test]
fn dates_times_and_cron_as_the_form_writes_them() {
    let m = map(json!({
        "dt": "2026-11-12T03:04:05", "dt2": "2026-11-12 03:04", "bad": "2026-02-30T00:00:00",
        "junk": "mañana", "d": "2026-11-12", "bd": "12/11/2026", "t": "7:05", "bt": "24:00",
        "c5": "0  3 * * MON-FRI", "c7": "0 0 3 * * ? *", "cbad": "0 3 * *", "cq": "0 3 * * 'x'"
    }));
    let v = Vals(&m);
    let ms = datetime(v, "dt", "Inicio").unwrap().unwrap();
    assert_eq!(dt_value(ms), "2026-11-12T03:04:05");
    assert_eq!(
        dt_value(datetime(v, "dt2", "Inicio").unwrap().unwrap()),
        "2026-11-12T03:04:00"
    );
    assert!(refused(datetime(v, "bad", "Inicio")).contains("no es una fecha y hora válida"));
    assert!(refused(datetime(v, "junk", "Inicio")).contains("«Inicio»"));
    assert_eq!(
        date_value(date(v, "d", "Fecha").unwrap().unwrap()),
        "2026-11-12"
    );
    assert!(refused(date(v, "bd", "Fecha")).contains("AAAA-MM-DD"));
    assert_eq!(time(v, "t", "Hora").unwrap(), Some(7 * 3600 + 5 * 60));
    assert!(refused(time(v, "bt", "Hora")).contains("HH:MM:SS"));
    assert_eq!(time_text(3 * 3600 + 7), "03:00:07");
    assert_eq!(
        cron(v, "c5", "Calendario", 5..=5).unwrap().as_deref(),
        Some("0 3 * * MON-FRI")
    );
    assert_eq!(
        cron(v, "c7", "Cron", 6..=7).unwrap().as_deref(),
        Some("0 0 3 * * ? *")
    );
    assert!(refused(cron(v, "cbad", "Calendario", 5..=5)).contains("5 campos"));
    assert!(refused(cron(v, "c5", "Cron", 6..=7)).contains("6 o 7 campos"));
    assert!(refused(cron(v, "cq", "Calendario", 5..=5)).contains("no es una expresión cron"));
    assert_eq!(cron(v, "missing", "C", 5..=5).unwrap(), None);
}

#[test]
fn a_script_shows_comments_and_ends_each_statement() {
    let mut p = Plan::new("Hace algo\ncon dos líneas");
    p.exec("UPDATE t SET a = 1".into());
    p.comment("y después");
    p.query("SELECT f()".into());
    assert!(p.changes());
    assert_eq!(
        p.script(),
        "-- Hace algo con dos líneas\nUPDATE t SET a = 1;\n-- y después\nSELECT f();\n"
    );
    let n = Plan::nothing("x", "7".into());
    assert!(!n.changes());
    assert_eq!(n.script(), "-- «x»: no hay cambios que guardar.\n");
}

#[test]
fn an_atomic_plan_runs_in_one_transaction_and_rolls_back_on_a_failure() {
    let mut p = Plan::new("dos pasos");
    p.atomic = true;
    p.exec("INSERT a".into());
    p.exec("INSERT b".into());
    p.id = Some("1".into());
    let mut f = Fake::default();
    assert_eq!(execute(&mut f, p).unwrap().as_deref(), Some("1"));
    assert_eq!(f.tx, ["begin", "commit"]);
    assert!(f.sent("INSERT a") && f.sent("INSERT b"));

    let mut p = Plan::new("dos pasos");
    p.atomic = true;
    p.exec("INSERT a".into());
    p.exec("INSERT b".into());
    let mut f = Fake::default().fail("INSERT a", Some("-268"));
    assert!(execute(&mut f, p).is_err());
    assert_eq!(f.tx, ["begin", "rollback"]);
    assert!(!f.sent("INSERT b"));
}

#[test]
fn a_plan_without_changes_runs_nothing_and_answers_the_id() {
    let mut f = Fake::default();
    assert_eq!(
        execute(&mut f, Plan::nothing("x", "9".into()))
            .unwrap()
            .as_deref(),
        Some("9")
    );
    assert!(f.log.is_empty() && f.tx.is_empty());
}

#[test]
fn the_new_id_is_looked_up_after_the_steps_and_a_failed_lookup_keeps_the_save() {
    let mut p = Plan::new("crea");
    p.exec("CREATE x".into());
    p.lookup = Some(Box::new(|src: &mut dyn JobSource| {
        let s = src.query("SELECT id", &[])?;
        Ok(s.get(0, "id").map(str::to_string))
    }));
    let mut f = Fake::default().rows("SELECT id", &["id"], vec![vec![Some("42")]]);
    assert_eq!(execute(&mut f, p).unwrap().as_deref(), Some("42"));
    assert_eq!(f.tx, ["commit"]);
    assert_eq!(f.log[0].0, "CREATE x");

    let mut p = Plan::new("crea");
    p.exec("CREATE x".into());
    p.lookup = Some(Box::new(|src: &mut dyn JobSource| {
        src.query("SELECT id", &[])?;
        Ok(None)
    }));
    let mut f = Fake::default().fail("SELECT id", None);
    assert_eq!(execute(&mut f, p).unwrap(), None);
}

#[test]
fn output_parameters_are_checked_by_the_step() {
    fn ok_unless_12(o: &[Option<String>]) -> Result<Option<String>> {
        match o.first().and_then(|v| v.as_deref()) {
            Some("12") => Err(Error::Query(o[1].clone().unwrap_or_default())),
            _ => Ok(None),
        }
    }
    let mut p = Plan::new("llama");
    p.call_out(
        "CALL P(?, ?)".into(),
        vec![Param::Out, Param::Out],
        ok_unless_12,
    );
    let mut f = Fake::default().out("CALL P", vec![Some("12"), Some("no existe la tarea")]);
    assert_eq!(refused(execute(&mut f, p)), "no existe la tarea");
    assert_eq!(f.tx, ["rollback"]);
}

#[test]
fn bound_values_never_go_into_the_text_and_a_value_read_back_is_the_new_id() {
    fn named(o: &[Option<String>]) -> Result<Option<String>> {
        Ok(o.first().cloned().flatten())
    }
    let mut p = Plan::new("crea");
    p.id = Some("PEDIDO".into());
    p.call_out(
        "CALL P('u', ?, ?, ?)".into(),
        vec![
            Param::In(Some("s3cr'et".into())),
            Param::InOut(Some("PEDIDO".into())),
            Param::Out,
        ],
        named,
    );
    let mut f = Fake::default().out("CALL P", vec![Some("DADO"), None]);
    assert_eq!(execute(&mut f, p).unwrap().as_deref(), Some("DADO"));
    assert_eq!(f.log[0].0, "CALL P('u', ?, ?, ?)");
    assert_eq!(f.log[0].1, ["s3cr'et", "PEDIDO"]);
}

#[test]
fn a_lookup_that_finishes_the_save_fails_it() {
    let mut p = Plan::new("crea");
    p.exec("CREATE x".into());
    p.lookup = Some(Box::new(|_: &mut dyn JobSource| {
        Err(Error::Query("no se pudo retener".into()))
    }));
    p.lookup_required = true;
    let mut f = Fake::default();
    assert_eq!(refused(execute(&mut f, p)), "no se pudo retener");
    // The steps ran and were committed: the message says what's left.
    assert_eq!(f.tx, ["commit"]);
}

#[test]
fn engines_without_a_scheduler_log_say_so() {
    for id in ["db2i", "hive", "vertica"] {
        let mut f = Fake::default();
        assert!(matches!(
            logs(preset(id), &mut f),
            Err(Error::Unsupported(_))
        ));
    }
}

#[test]
fn engines_that_keep_no_history_to_clear_say_so() {
    for id in ["db2zos", "db2i", "hive", "vertica"] {
        let mut f = Fake::default();
        assert!(
            matches!(
                clear_history(preset(id), &mut f, "1"),
                Err(Error::Unsupported(_))
            ),
            "{id}"
        );
        assert!(f.log.is_empty(), "{id}");
    }
}

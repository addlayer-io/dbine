//! Vertica's scheduled triggers.

use super::*;
use crate::jobs::vertica::fixtures::*;

const TRIGGER_COLS: &[&str] = &[
    "trigger_name",
    "schema_name",
    "owner",
    "procedure_name",
    "procedure_args",
    "enabled",
];

fn log_user_actions() -> Vec<Option<&'static str>> {
    vec![
        Some("log_user_actions"),
        Some("management"),
        Some("dbadmin"),
        Some("log_user_count"),
        None,
        Some("1"),
    ]
}

fn odd() -> Vec<Option<&'static str>> {
    vec![
        Some("Odd \"Name\""),
        Some("My Schema"),
        Some("bob"),
        Some("p"),
        None,
        Some("t"),
    ]
}

fn triggers() -> Vec<Vec<Option<&'static str>>> {
    vec![
        log_user_actions(),
        vec![
            Some("raise_trigger"),
            Some("public"),
            Some("dbadmin"),
            Some("raiseXY"),
            // As the catalog keeps 'it''s': not the same value back.
            Some("7, 'it's'"),
            Some("0"),
        ],
        odd(),
    ]
}

fn fake(superuser: &'static str, one: Vec<Option<&'static str>>, time_table: bool) -> Fake {
    fake_with(
        superuser,
        one,
        time_table,
        "1",
        vec![vec![Some("management"), Some("log_user_count")]],
    )
}

fn fake_with(
    superuser: &'static str,
    one: Vec<Option<&'static str>>,
    time_table: bool,
    config: &'static str,
    procedures: Vec<Vec<Option<&'static str>>>,
) -> Fake {
    // The next starts as the server turns them into UTC.
    let tt = if time_table {
        vec![
            vec![
                Some("daily_1am"),
                Some("log_user_actions"),
                Some("2026-11-13 04:00:00"),
            ],
            // The time table keeps disabled triggers too.
            vec![
                Some("biannual"),
                Some("raise_trigger"),
                Some("2030-01-01 12:00:00"),
            ],
        ]
    } else {
        Vec::new()
    };
    Fake::default()
        .rows(TRIGGERS, TRIGGER_COLS, triggers())
        .rows(ONE, TRIGGER_COLS, vec![one])
        .rows(
            SCHEDULES,
            &[
                "schedule_name",
                "schema_name",
                "owner",
                "attached_trigger",
                "enabled",
                "date_time_type",
                "date_time_string",
            ],
            vec![
                vec![
                    Some("daily_1am"),
                    Some("management"),
                    Some("dbadmin"),
                    Some("log_user_actions"),
                    Some("1"),
                    Some("CRON"),
                    Some("0 1 * * *"),
                ],
                vec![
                    Some("biannual"),
                    Some("public"),
                    Some("dbadmin"),
                    Some("raise_trigger"),
                    Some("1"),
                    Some("DATE_TIME_LIST"),
                    Some("2026-01-01 12:00:00-00, 2026-06-01 12:00:00-03"),
                ],
            ],
        )
        .rows(
            TIME_TABLE_UTC,
            &["schedule_name", "attached_trigger", "next_utc"],
            tt,
        )
        .rows(PROCEDURES, &["schema_name", "procedure_name"], procedures)
        .rows(SUPERUSER, &["S"], vec![vec![Some(superuser)]])
        .rows(CONFIG, &["V"], vec![vec![Some(config)]])
        .rows(ASN, &["ASN"], vec![vec![Some("v_vmart_node0001")]])
        .rows(ME, &["ME"], vec![vec![Some("v_vmart_node0002")]])
}

#[test]
fn ids_are_two_quoted_parts() {
    for (a, b) in [
        ("public", "t"),
        ("My Schema", "Odd \"Name\""),
        ("n1.s", "a.b"),
    ] {
        let id = quote_pair(a, b);
        assert_eq!(
            parse_pair(&id).unwrap(),
            (a.to_string(), b.to_string()),
            "{id}"
        );
    }
    assert_eq!(quote_pair("s", "x\"y"), "\"s\".\"x\"\"y\"");
    for bad in [
        "public.t",
        "\"public\".t",
        "\"public\".\"t\"x",
        "\"a\".\"b\".\"c\"",
        "\"\".\"b\"",
        "\"a\"",
        "\"a\".\"b",
        "\"a\"\"\".\"b\"\"",
        "\"a\u{0}\".\"b\"",
    ]
    .iter()
    .chain(FORGED)
    {
        forged(parse_pair(bad).unwrap_err());
    }
    assert!(parse_pair(&quote_pair("a", &"b".repeat(257))).is_err());
}

#[test]
fn timestamps_with_their_offset_turn_into_utc() {
    let utc = |s: &str| parse_ts_utc(s).map(rfc3339);
    assert_eq!(
        utc("2026-11-13 01:00:00-03").as_deref(),
        Some("2026-11-13T04:00:00Z")
    );
    assert_eq!(
        utc("2026-11-13 01:00:00+05:30").as_deref(),
        Some("2026-11-12T19:30:00Z")
    );
    assert_eq!(
        utc("2026-11-13 01:00:00.5-0300").as_deref(),
        Some("2026-11-13T04:00:00Z")
    );
    assert_eq!(
        utc("2026-11-13 01:00:00-00").as_deref(),
        Some("2026-11-13T01:00:00Z")
    );
    assert_eq!(
        utc("2026-11-13 01:00:00Z").as_deref(),
        Some("2026-11-13T01:00:00Z")
    );
    assert_eq!(
        utc("2026-11-13 01:00:00").as_deref(),
        Some("2026-11-13T01:00:00Z")
    );
    // Db2's dashes and dots aren't an offset.
    assert_eq!(
        utc("2026-11-13-01.00.00.000000").as_deref(),
        Some("2026-11-13T01:00:00Z")
    );
    assert_eq!(utc("soon"), None);
}

#[test]
fn vertica_lists_triggers_with_their_schedules() {
    let mut f = fake("t", log_user_actions(), true);
    let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
    assert!(l.notes.is_empty(), "{:?}", l.notes);
    let by = |n: &str| l.jobs.iter().find(|j| j.name == n).unwrap();
    let j = by("log_user_actions");
    assert_eq!(j.id, "\"management\".\"log_user_actions\"");
    assert_eq!(j.kind, "Trigger");
    assert_eq!(j.group.as_deref(), Some("management"));
    assert_eq!(j.owner.as_deref(), Some("dbadmin"));
    assert_eq!(j.schedule.as_deref(), Some("USING CRON '0 1 * * *'"));
    assert_eq!(j.next_run.as_deref(), Some("2026-11-13T04:00:00Z"));
    assert!(j.enabled && j.can_run && j.can_enable && j.has_script);
    assert!(!j.can_stop && !j.has_history && !j.running);
    let r = by("raise_trigger");
    assert_eq!(
        r.schedule.as_deref(),
        Some("USING DATETIMES('2026-01-01 12:00:00-00', '2026-06-01 12:00:00-03')")
    );
    assert!(!r.enabled);
    assert_eq!(r.next_run, None, "disabled");
    assert!(
        !r.has_script,
        "the procedure takes arguments the catalog doesn't keep"
    );
    let o = by("Odd \"Name\"");
    assert_eq!(o.id, "\"My Schema\".\"Odd \"\"Name\"\"\"");
    assert_eq!(o.schedule, None);
    assert!(o.enabled && !o.has_script);
}

#[test]
fn vertica_notes() {
    // Not on the scheduler's node: no next starts, and where to see them.
    let mut f = fake("t", log_user_actions(), false);
    let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.iter().all(|j| j.next_run.is_none()));
    assert_eq!(
        l.notes,
        vec![
            "La próxima ejecución de cada trigger se ve conectado al nodo v_vmart_node0001, que coordina el planificador."
                .to_string()
        ]
    );
    // The scheduler is off.
    let mut f = fake_with("t", log_user_actions(), true, "0", Vec::new());
    let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
    assert!(
        l.notes[0].contains("EnableStoredProcedureScheduler"),
        "{:?}",
        l.notes
    );
    // Before 11.1.1.
    let mut f = Fake::default()
        .fail(TRIGGERS, None)
        .rows(TABLE, &["S"], Vec::new());
    let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
    assert!(
        l.jobs.is_empty() && l.notes[0].contains("11.1.1"),
        "{:?}",
        l.notes
    );
    // The table is there and a superuser can't read it: a real failure.
    let mut f = Fake::default()
        .fail(TRIGGERS, None)
        .rows(TABLE, &["S"], vec![vec![Some("t")]])
        .rows(SUPERUSER, &["S"], vec![vec![Some("t")]]);
    assert!(list(preset("vertica"), &mut f, &ctx("")).is_err());
}

const SUPERUSER_ONLY: &str = "Solo un superusuario ve los triggers programados de Vertica.";

#[test]
fn vertica_says_what_someone_who_isnt_a_superuser_sees() {
    // The catalog refuses them: empty, and why.
    for (super_only, table) in [
        ("t", true),
        ("f", true),
        ("t", false), // system_tables unreadable too
    ] {
        let mut f = Fake::default().fail(TRIGGERS, None);
        f = if table {
            f.rows(TABLE, &["S"], vec![vec![Some(super_only)]])
        } else {
            f.fail(TABLE, None)
        }
        .rows(SUPERUSER, &["S"], vec![vec![Some("f")]]);
        let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
        assert!(l.jobs.is_empty());
        assert_eq!(l.notes, vec![SUPERUSER_ONLY.to_string()]);
    }

    // The catalog answers them with every trigger (as Vertica 25.4 does,
    // in every schema): nothing to say.
    let mut f = fake("f", log_user_actions(), true).rows(TABLE, &["S"], vec![vec![Some("f")]]);
    let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
    assert_eq!(l.jobs.len(), 3);
    assert!(l.notes.is_empty(), "{:?}", l.notes);
    // A superuser-only table answers them with what it lets them see:
    // said even when that's nothing.
    for triggers in [Vec::new(), vec![log_user_actions()]] {
        let mut f = Fake::default()
            .rows(TRIGGERS, TRIGGER_COLS, triggers)
            .rows(TABLE, &["S"], vec![vec![Some("t")]])
            .rows(SUPERUSER, &["S"], vec![vec![Some("f")]]);
        let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
        assert_eq!(l.notes[0], SUPERUSER_ONLY, "{:?}", l.notes);
    }

    // A superuser gets no such note, and nobody asks system_tables.
    let mut f = fake("t", log_user_actions(), true);
    let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
    assert!(l.notes.is_empty(), "{:?}", l.notes);
    assert!(!f.sent("system_tables"));
}

#[test]
fn vertica_next_starts_come_in_utc_from_the_server() {
    let mut f = fake("t", log_user_actions(), true);
    list(preset("vertica"), &mut f, &ctx("")).unwrap();
    assert!(f.sent("scheduled_execution_time AT TIME ZONE 'UTC' AS next_utc"));
    assert!(!f.sent(TIME_TABLE));
    // A server where that column has another name: the table as it is,
    // with the offset each value carries.
    let mut f = fake("t", log_user_actions(), true)
        .before(TIME_TABLE_UTC, Reply::Fail(None, "falló"))
        .rows(
            TIME_TABLE,
            &["schedule_name", "attached_trigger", "scheduled_time"],
            vec![vec![
                Some("daily_1am"),
                Some("log_user_actions"),
                Some("2026-11-13 01:00:00-03"),
            ]],
        );
    let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
    let j = l
        .jobs
        .iter()
        .find(|j| j.name == "log_user_actions")
        .unwrap();
    assert_eq!(j.next_run.as_deref(), Some("2026-11-13T04:00:00Z"));
}

#[test]
fn vertica_runs_and_enables_with_catalog_names() {
    let id = "\"management\".\"log_user_actions\"";
    let mut f = fake("t", log_user_actions(), true);
    run(preset("vertica"), &mut f, &ctx(""), id).unwrap();
    assert_eq!(
        f.log[0].1,
        vec!["management".to_string(), "log_user_actions".to_string()],
        "the names are bound"
    );
    assert_eq!(
        f.last().0,
        "SELECT EXECUTE_TRIGGER('management.log_user_actions')"
    );
    set_enabled(preset("vertica"), &mut f, id, false).unwrap();
    assert_eq!(
        f.last().0,
        "SELECT ENABLE_TRIGGER('management.log_user_actions', false)"
    );
    set_enabled(preset("vertica"), &mut f, id, true).unwrap();
    assert_eq!(
        f.last().0,
        "SELECT ENABLE_TRIGGER('management.log_user_actions', true)"
    );

    // A name that needs quotes goes quoted, inside the literal.
    let mut f = fake("t", odd(), true);
    run(
        preset("vertica"),
        &mut f,
        &ctx(""),
        &quote_pair("My Schema", "Odd \"Name\""),
    )
    .unwrap();
    assert_eq!(
        f.last().0,
        "SELECT EXECUTE_TRIGGER('\"My Schema\".\"Odd \"\"Name\"\"\"')"
    );

    // Not a superuser: nothing is sent.
    let mut f = fake("f", log_user_actions(), true);
    let l = list(preset("vertica"), &mut f, &ctx("")).unwrap();
    assert!(l.jobs.iter().all(|j| !j.can_run && !j.can_enable));
    for e in [
        run(preset("vertica"), &mut f, &ctx(""), id).unwrap_err(),
        set_enabled(preset("vertica"), &mut f, id, true).unwrap_err(),
    ] {
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }
    assert!(!f.sent("EXECUTE_TRIGGER(") && !f.sent("ENABLE_TRIGGER("));

    // Unknown and forged ids.
    let mut f = Fake::default().rows(ONE, TRIGGER_COLS, Vec::new());
    let e = run(preset("vertica"), &mut f, &ctx(""), "\"x'); DROP\".\"t\"").unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.contains("no existe")),
        "{e:?}"
    );
    only_bound(&f, "x'); DROP");
    let mut f = fake("t", log_user_actions(), true);
    for bad in FORGED.iter().chain(&["management.log_user_actions"]) {
        forged(run(preset("vertica"), &mut f, &ctx(""), bad).unwrap_err());
        forged(set_enabled(preset("vertica"), &mut f, bad, true).unwrap_err());
        forged(detail(preset("vertica"), &mut f, &ctx(""), bad).unwrap_err());
        forged(script(preset("vertica"), &mut f, bad).unwrap_err());
    }
    assert!(f.log.is_empty());
    for e in [
        stop(preset("vertica"), &mut f, id).unwrap_err(),
        runs(preset("vertica"), &mut f, id, 5).unwrap_err(),
    ] {
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }
}

#[test]
fn vertica_detail_and_script() {
    let id = "\"management\".\"log_user_actions\"";
    let mut f = fake("t", log_user_actions(), true);
    let d = detail(preset("vertica"), &mut f, &ctx(""), id).unwrap();
    assert_eq!(d.job.id, id);
    assert_eq!(d.steps[0].command.as_deref(), Some("log_user_count()"));
    let label = |l: &str| {
        d.properties
            .iter()
            .find(|p| p.label == l)
            .map(|p| p.value.as_str())
    };
    assert_eq!(label("Programación"), Some("management.daily_1am"));
    let sc = script(preset("vertica"), &mut f, id).unwrap();
    assert_eq!(
        sc,
        "CREATE SCHEDULE \"management\".\"daily_1am\" USING CRON '0 1 * * *';
CREATE TRIGGER \"management\".\"log_user_actions\" ON SCHEDULE \"management\".\"daily_1am\" EXECUTE PROCEDURE \"management\".\"log_user_count\"() AS DEFINER;
ALTER TRIGGER \"management\".\"log_user_actions\" OWNER TO \"dbadmin\";
"
    );
    assert_eq!(f.last().1, vec!["log_user_count".to_string()], "bound");

    // A disabled trigger is disabled again, with its arguments.
    let mut raise = triggers()[1].clone();
    raise[4] = Some("7,'x' , NULL,-1.5e3");
    let two = vec![
        vec![Some("public"), Some("raiseXY")],
        vec![Some("other"), Some("raiseXY")],
    ];
    let mut f = fake_with("t", raise, true, "1", two);
    let sc = script(preset("vertica"), &mut f, "\"public\".\"raise_trigger\"").unwrap();
    assert!(sc.starts_with(
        "CREATE SCHEDULE \"public\".\"biannual\" USING DATETIMES('2026-01-01 12:00:00-00', '2026-06-01 12:00:00-03');\n"
    ), "{sc}");
    // Two procedures by that name: left unqualified.
    assert!(
        sc.contains("EXECUTE PROCEDURE \"raiseXY\"(7, 'x', NULL, -1.5e3) AS DEFINER;"),
        "{sc}"
    );
    assert!(
        sc.ends_with("SELECT ENABLE_TRIGGER('public.raise_trigger', false);\n"),
        "{sc}"
    );
    // Arguments that don't read back: no script.
    let mut f = fake("t", triggers()[1].clone(), true);
    let d = detail(
        preset("vertica"),
        &mut f,
        &ctx(""),
        "\"public\".\"raise_trigger\"",
    )
    .unwrap();
    assert_eq!(d.steps[0].command.as_deref(), Some("raiseXY(7, 'it's')"));
    assert!(!d.job.has_script);
    let e = script(preset("vertica"), &mut f, "\"public\".\"raise_trigger\"").unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    // No schedule found for it.
    let mut f = fake("t", odd(), true);
    let e = script(
        preset("vertica"),
        &mut f,
        &quote_pair("My Schema", "Odd \"Name\""),
    )
    .unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
}

#[test]
fn trigger_arguments_go_back_only_when_they_read_the_same() {
    for (v, want) in [
        (None, Some("")),
        (Some(""), Some("")),
        (Some("7"), Some("7")),
        (Some("7, 'x'"), Some("7, 'x'")),
        (
            Some(" 'a b' ,2.5 ,null, TRUE "),
            Some("'a b', 2.5, null, TRUE"),
        ),
        (Some("''"), Some("''")),
        (Some("7, 'it's'"), None),
        (Some("'unterminated"), None),
        (Some("7,"), None),
        (Some("now()"), None),
        (Some("1; DROP TABLE x"), None),
        (Some("'a' 'b'"), None),
        (Some("1e999"), None),
    ] {
        assert_eq!(safe_args(v).as_deref(), want, "{v:?}");
    }
}

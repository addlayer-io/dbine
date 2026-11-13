//! SAP ASE: Job Scheduler's scheduled jobs, jobs and schedules.

use super::*;
use crate::scheduler::ase as e;

const SJ_COLS: &[&str] = &[
    "sjob_id",
    "sjob_name",
    "sjob_owner",
    "sjob_state",
    "job_name",
    "sched_name",
    "sjob_server",
    "sjob_timeout",
];
const J_COLS: &[&str] = &[
    "job_id",
    "job_name",
    "job_desc",
    "job_owner",
    "job_properties",
    "job_default_timeout",
];
const S_COLS: &[&str] = &[
    "sched_id",
    "sched_name",
    "sched_desc",
    "sched_owner",
    "sched_interval",
    "sched_units",
    "sched_startdate",
    "sched_starttime",
    "sched_enddate",
    "sched_endtime",
    "sched_days",
    "sched_dates",
];

/// Scheduled job 7 `nightly` (job 3 `load_sales`, schedule 4 `daily`),
/// schedule 5 `weekly` unused; the login `sa` has js_admin_role when
/// `admin`.
fn server(admin: bool) -> Fake {
    server_with(admin, false)
}

/// [`server`], with scheduled job 9 `weekly_report` too when `new_one`.
fn server_with(admin: bool, new_one: bool) -> Fake {
    let sj = vec![
        Some("7"),
        Some("nightly"),
        Some("sa"),
        Some("enabled"),
        Some("load_sales"),
        Some("daily"),
        Some("SYB1"),
        Some("30"),
    ];
    Fake::default()
        .rows("db_id('sybmgmtdb')", &["DBID"], vec![vec![Some("5")]])
        .rows(
            "proc_role",
            &["ADM", "USR"],
            vec![vec![Some(if admin { "1" } else { "0" }), Some("1")]],
        )
        .rows("enable job scheduler", &["VALUE"], vec![vec![Some("1")]])
        .rows(
            "suser_name()",
            &["ME"],
            vec![vec![Some(if admin { "sa" } else { "bob" })]],
        )
        .rows("sp_sjobhelp @name = '7'", SJ_COLS, vec![sj.clone()])
        .rows("'list,scheduled", SJ_COLS, {
            let mut all = vec![sj];
            if new_one {
                all.push(vec![
                    Some("9"),
                    Some("weekly_report"),
                    Some("sa"),
                    None,
                    Some("load_sales"),
                    Some("daily"),
                    None,
                    None,
                ]);
            }
            all
        })
        .rows(
            "'list,jobs",
            J_COLS,
            vec![vec![
                Some("3"),
                Some("load_sales"),
                Some("carga"),
                Some("sa"),
                Some("shared"),
                Some("60"),
            ]],
        )
        .rows(
            "'list,schedules",
            S_COLS,
            vec![
                vec![
                    Some("4"),
                    Some("daily"),
                    None,
                    Some("sa"),
                    Some("2"),
                    Some("hours"),
                    Some("2026-11-12 00:00:00.000"),
                    Some("10:00"),
                    None,
                    Some("4:00PM"),
                    Some("Monday:Friday"),
                    None,
                ],
                vec![
                    Some("5"),
                    Some("weekly"),
                    None,
                    Some("sa"),
                    Some("1day"),
                    None,
                    None,
                    Some("1900-01-01 23:00:00.000"),
                    None,
                    None,
                    None,
                    None,
                ],
            ],
        )
        .rows(
            "sp_sjobcmd @name = 'jname=3'",
            &["text"],
            vec![vec![Some("truncate table t\n")], vec![Some("go\n")]],
        )
        .rows(
            "master..sysservers",
            &["srvname"],
            vec![vec![Some("SYB1")], vec![Some("SYB2")]],
        )
}

fn form_of(k: &str, id: &str) -> ObjectForm {
    form(preset("sybase"), &mut server(true), &ctx(), k, Some(id)).unwrap()
}

#[test]
fn a_scheduled_job_runs_now_stops_and_clears_its_history() {
    let i = info(preset("sybase"), &mut server(true), &ctx()).unwrap();
    let sj = &i.kinds[0];
    let keys: Vec<&str> = sj.actions.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, ["run", "stop"]);
    assert!(sj.clear_history && !sj.run_from_step);
    // Jobs and schedules are run through the scheduled jobs that use them.
    for k in &i.kinds[1..] {
        assert!(k.actions.is_empty() && !k.clear_history, "{}", k.key);
    }
    assert_eq!(
        action_script(preset("sybase"), &mut server(true), &ctx(), "job", "7", "run").unwrap(),
        "-- Job Scheduler corre el job programado ahora (run_now).\nexec sybmgmtdb..sp_sjobcontrol @name = '7', @option = 'run_now,all_users'\ngo\n"
    );
    // Without js_admin_role, only one's own: no all_users.
    let mut f = server(false);
    action(preset("sybase"), &mut f, &ctx(), "job", "7", "stop").unwrap();
    assert!(f.sent("exec sybmgmtdb..sp_sjobcontrol @name = '7', @option = 'terminate'"));
    for (kind, id, act) in [
        ("jobdef", "3", "run"),
        ("job", "7; drop", "run"),
        ("job", "7", "pause"),
    ] {
        let mut f = server(true);
        assert!(
            action_script(preset("sybase"), &mut f, &ctx(), kind, id, act).is_err(),
            "{kind} {id} {act}"
        );
        assert!(!f.sent("sp_sjobcontrol"));
    }
}

#[test]
fn a_new_scheduled_job_links_a_job_and_a_schedule() {
    let v = fv(
        json!({
            "name": "weekly_report", "enabled": false, "job": "load_sales", "schedule": "daily",
            "server": "SYB2", "timeout": 45, "locale": "us_english"
        }),
        json!({}),
    );
    assert_eq!(
        script(preset("sybase"), &mut server(true), "job", None, &v).unwrap(),
        "-- Job programado «weekly_report» de Job Scheduler
-- ⚠ El job programado se crea habilitado y se deshabilita con otra llamada: si esa falla, queda habilitado y va a correr.
-- ⚠ Las sentencias corren una por una, sin transacción: si una falla, las anteriores quedan hechas.
exec sybmgmtdb..sp_sjobcreate @name = 'sjname=weekly_report', @option = 'jname=load_sales,sname=daily,server=SYB2,timeout=45'
go
exec sybmgmtdb..sp_sjobcontrol @name = 'weekly_report', @option = 'disable,all_users'
go
exec sybmgmtdb..sp_sjobmodify @name = 'sjname=weekly_report', @option = 'locale=us_english'
go
"
    );
    let mut f = server_with(true, true);
    let id = save(preset("sybase"), &mut f, "job", None, &v).unwrap();
    assert_eq!(id.as_deref(), Some("9"));
    assert!(f.sent("sp_sjobcreate @name = 'sjname=weekly_report'"));
    // The procedures run one by one (no transaction around them).
    assert!(!f.tx.contains(&"begin"));
}

#[test]
fn an_edit_is_one_sp_sjobmodify_with_what_changed() {
    let f = form_of("job", "7");
    well_formed(&f);
    assert_eq!(val(&f, "job"), "load_sales");
    assert_eq!(val(&f, "schedule"), "daily");
    assert_eq!(val(&f, "timeout"), 30);
    assert_eq!(val(&f, "state"), "enabled");
    assert!(!f.read_only);
    let v = edit(
        &f,
        json!({"name": "nightly2", "enabled": false, "timeout": 60}),
    );
    assert_eq!(
        script(preset("sybase"), &mut server(true), "job", Some("7"), &v).unwrap(),
        "-- Job programado «nightly» de Job Scheduler
-- ⚠ Las sentencias corren una por una, sin transacción: si una falla, las anteriores quedan hechas.
exec sybmgmtdb..sp_sjobmodify @name = '7', @option = 'sjname=nightly2,timeout=60'
go
exec sybmgmtdb..sp_sjobcontrol @name = '7', @option = 'disable,all_users'
go
"
    );
    // Only the switch: sp_sjobcontrol alone.
    assert_eq!(
        script(
            preset("sybase"),
            &mut server(true),
            "job",
            Some("7"),
            &edit(&f, json!({"enabled": false}))
        )
        .unwrap(),
        "-- Job programado «nightly» de Job Scheduler
exec sybmgmtdb..sp_sjobcontrol @name = '7', @option = 'disable,all_users'
go
"
    );
    assert_eq!(
        script(
            preset("sybase"),
            &mut server(true),
            "job",
            Some("7"),
            &edit(&f, json!({}))
        )
        .unwrap(),
        "-- «nightly»: no hay cambios que guardar.\n"
    );
    let m = refused(script(
        preset("sybase"),
        &mut server(true),
        "job",
        Some("7"),
        &edit(&f, json!({"timeout": null})),
    ));
    assert!(m.contains("no lo deja vacío"), "{m}");
}

#[test]
fn another_job_or_schedule_drops_and_creates_the_scheduled_job() {
    let f = form_of("job", "7");
    let s = script(
        preset("sybase"),
        &mut server(true),
        "job",
        Some("7"),
        &edit(&f, json!({"schedule": "weekly"})),
    )
    .unwrap();
    let drop = s
        .find("exec sybmgmtdb..sp_sjobdrop @name = '7', @option = 'all_users'")
        .unwrap();
    let create = s
        .find("sp_sjobcreate @name = 'sjname=nightly', @option = 'jname=load_sales,sname=weekly,server=SYB1,timeout=30'")
        .unwrap();
    assert!(drop < create, "{s}");
    assert_eq!(
        warnings(&s),
        [
            "Job Scheduler no cambia el job ni el calendario de un job programado: se borra y se crea de nuevo, y recibe otro id.",
            "Su historial queda con el id anterior: el job programado nuevo empieza sin historial.",
            "Las llamadas corren una por una: si la creación falla, el job programado queda borrado.",
        ],
        "{s}"
    );
    // Disabled: created enabled, then switched off by another call.
    let s = script(
        preset("sybase"),
        &mut server(true),
        "job",
        Some("7"),
        &edit(&f, json!({"schedule": "weekly", "enabled": false})),
    )
    .unwrap();
    assert!(
        s.find("sp_sjobcreate").unwrap()
            < s.find("sp_sjobcontrol @name = 'nightly', @option = 'disable,all_users'")
                .unwrap(),
        "{s}"
    );
    assert_eq!(
        warnings(&s)[3..],
        ["El job programado se crea habilitado y se deshabilita con otra llamada: si esa falla, queda habilitado y va a correr."],
        "{s}"
    );
}

#[test]
fn a_job_keeps_its_sql_in_pieces_with_sp_sjobcmd() {
    let long = "x".repeat(300);
    let v = fv(
        json!({"name": "purge", "description": "borra lo viejo", "timeout": 120, "properties": "shared",
               "command": format!("delete from t where note = 'it''s'\n{long}")}),
        json!({}),
    );
    let s = script(preset("sybase"), &mut server(true), "jobdef", None, &v).unwrap();
    assert!(s.starts_with(
        "-- Job «purge» de Job Scheduler
-- ⚠ Las sentencias corren una por una, sin transacción: si una falla, las anteriores quedan hechas.
exec sybmgmtdb..sp_sjobcreate @name = 'jname=purge', @option = 'jdesc=borra lo viejo,jproperties=shared,default_timeout=120'
go
exec sybmgmtdb..sp_sjobcmd @name = 'jname=purge', @option = 'add', @text = 'delete from t where note = ''it''''s''"
    ), "{s}");
    assert_eq!(s.matches("@option = 'add'").count(), 2, "{s}");
    assert!(
        s.contains("xxxx\n'\ngo\n"),
        "the text ends in a line break: {s}"
    );
    let m = refused(script(
        preset("sybase"),
        &mut server(true),
        "jobdef",
        None,
        &fv(
            json!({"name": "purge", "description": "a, b", "command": "select 1"}),
            json!({}),
        ),
    ));
    assert!(m.contains("comas"), "{m}");
    for name in ["1purge", "a=b", "a,b"] {
        let m = refused(script(
            preset("sybase"),
            &mut server(true),
            "jobdef",
            None,
            &fv(json!({"name": name, "command": "select 1"}), json!({})),
        ));
        assert!(m.contains("«Nombre»"), "{name}: {m}");
    }
}

#[test]
fn a_jobs_new_sql_drops_the_old_and_adds_the_new() {
    let f = form_of("jobdef", "3");
    well_formed(&f);
    assert_eq!(val(&f, "command"), "truncate table t\ngo");
    assert_eq!(val(&f, "used_by"), "nightly");
    let v = edit(
        &f,
        json!({"command": "truncate table u", "description": "otra"}),
    );
    assert_eq!(
        script(preset("sybase"), &mut server(true), "jobdef", Some("3"), &v).unwrap(),
        "-- Job «load_sales» de Job Scheduler
-- ⚠ El SQL del job se borra y se vuelve a cargar en llamadas sueltas: si una falla, el job queda sin su SQL o con una parte.
-- ⚠ Las sentencias corren una por una, sin transacción: si una falla, las anteriores quedan hechas.
exec sybmgmtdb..sp_sjobmodify @name = 'jname=3', @option = 'jdesc=otra'
go
exec sybmgmtdb..sp_sjobcmd @name = 'jname=3', @option = 'drop'
go
exec sybmgmtdb..sp_sjobcmd @name = 'jname=3', @option = 'add', @text = 'truncate table u
'
go
"
    );
    // Only the SQL: no sp_sjobmodify to leave done, its own warning only.
    let s = script(
        preset("sybase"),
        &mut server(true),
        "jobdef",
        Some("3"),
        &edit(&f, json!({"command": "truncate table u"})),
    )
    .unwrap();
    assert!(!s.contains("sp_sjobmodify"), "{s}");
    assert_eq!(
        warnings(&s),
        ["El SQL del job se borra y se vuelve a cargar en llamadas sueltas: si una falla, el job queda sin su SQL o con una parte."],
        "{s}"
    );
}

#[test]
fn a_schedule_reads_back_its_timing_and_changes_only_what_moved() {
    let f = form_of("schedule", "4");
    well_formed(&f);
    assert_eq!(val(&f, "repeats"), 2);
    assert_eq!(val(&f, "unit"), "hours");
    assert_eq!(val(&f, "start_date"), "2026-11-12");
    assert_eq!(val(&f, "start_time"), "10:00:00");
    assert_eq!(val(&f, "end_time"), "16:00:00");
    assert_eq!(val(&f, "on"), "days");
    assert_eq!(val(&f, "days"), json!(["Monday", "Friday"]));
    let v = edit(&f, json!({"start_time": "09:30"}));
    assert_eq!(
        script(
            preset("sybase"),
            &mut server(true),
            "schedule",
            Some("4"),
            &v
        )
        .unwrap(),
        "-- Calendario «daily» de Job Scheduler
exec sybmgmtdb..sp_sjobmodify @name = 'sname=4', @option = 'starttime=09:30'
go
"
    );
    // Taking a value away resets the timing and sets again what stays.
    let v = edit(
        &f,
        json!({"end_time": null, "on": "dates", "dates": ["1", "32"]}),
    );
    assert_eq!(
        script(preset("sybase"), &mut server(true), "schedule", Some("4"), &v).unwrap(),
        "-- Calendario «daily» de Job Scheduler
-- ⚠ Para quitar un valor, el calendario se vacía (reset=true) y se le vuelven a poner los que quedan: si eso falla, queda vacío.
exec sybmgmtdb..sp_sjobmodify @name = 'sname=4', @option = 'reset=true'
go
exec sybmgmtdb..sp_sjobmodify @name = 'sname=4', @option = 'repeats=2hours,startdate=20261112,starttime=10:00,dates=1:32'
go
"
    );
    let f = form_of("schedule", "5");
    assert_eq!(val(&f, "repeats"), 1);
    assert_eq!(val(&f, "unit"), "days");
    assert_eq!(val(&f, "start_time"), "23:00:00");
}

#[test]
fn a_new_schedule_and_its_checks() {
    let v = fv(
        json!({"name": "month_end", "description": "fin de mes", "repeats": 0, "start_time": "23:00",
               "on": "dates", "dates": ["32", "1"]}),
        json!({}),
    );
    assert_eq!(
        script(preset("sybase"), &mut server(true), "schedule", None, &v).unwrap(),
        "-- Calendario «month_end» de Job Scheduler
exec sybmgmtdb..sp_sjobcreate @name = 'sname=month_end', @option = 'sdesc=fin de mes,starttime=23:00,dates=1:32'
go
"
    );
    for (over, want) in [
        (
            json!({"repeats": 1, "unit": "days", "on": "days", "days": ["Monday"]}),
            "no lleva días ni fechas",
        ),
        (json!({"on": "days", "days": []}), "al menos un día"),
        (json!({"on": "days", "days": ["Funday"]}), "no es un día"),
        (json!({"start_time": "10:00:30"}), "sin segundos"),
        (
            json!({"start_date": "2026-11-20", "end_date": "2026-11-19"}),
            "anterior",
        ),
        (
            json!({"start_date": "2026-11-20", "end_date": "2026-11-20", "start_time": "10:00", "end_time": "09:00"}),
            "El mismo día",
        ),
        (json!({"unit": "weeks"}), "«Unidad»"),
    ] {
        let mut base = json!({"name": "s1", "repeats": 2, "unit": "hours"});
        for (k, x) in over.as_object().unwrap() {
            base[k] = x.clone();
        }
        let m = refused(e::schedule_from(&fv(base, json!({}))));
        assert!(m.contains(want), "{over}: {m}");
    }
}

#[test]
fn deletes_refuse_what_scheduled_jobs_still_use() {
    let m = refused(delete_script(
        preset("sybase"),
        &mut server(true),
        "jobdef",
        "3",
    ));
    assert!(m.contains("«nightly»"), "{m}");
    assert_eq!(
        delete_script(preset("sybase"), &mut server(true), "schedule", "5").unwrap(),
        "-- Borra el calendario «weekly» de Job Scheduler\nexec sybmgmtdb..sp_sjobdrop @name = 'sname=5', @option = 'all_users'\ngo\n"
    );
    assert_eq!(
        delete_script(preset("sybase"), &mut server(true), "job", "7").unwrap(),
        "-- Borra el job programado «nightly» de Job Scheduler\nexec sybmgmtdb..sp_sjobdrop @name = '7', @option = 'all_users'\ngo\n"
    );
}

#[test]
fn clearing_the_history_drops_it_with_sp_sjobhistory() {
    let mut f = server(true);
    clear_history(preset("sybase"), &mut f, "7").unwrap();
    assert!(f.sent("exec sybmgmtdb..sp_sjobhistory @name = '7', @option = 'drop,all_users'"));
    assert!(matches!(
        clear_history(preset("sybase"), &mut server(true), "7; drop"),
        Err(Error::Query(_))
    ));
}

#[test]
fn without_js_admin_role_others_objects_are_only_read() {
    let f = form(
        preset("sybase"),
        &mut server(false),
        &ctx(),
        "job",
        Some("7"),
    )
    .unwrap();
    assert!(f.read_only);
    assert!(f.notes[0].contains("otro usuario"));
    let v = edit(&f, json!({"enabled": false}));
    assert!(matches!(
        script(preset("sybase"), &mut server(false), "job", Some("7"), &v),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        delete_script(preset("sybase"), &mut server(false), "job", "7"),
        Err(Error::Unsupported(_))
    ));
    let i = info(preset("sybase"), &mut server(false), &ctx()).unwrap();
    assert!(i.notes.iter().any(|n| n.contains("js_admin_role")));
}

#[test]
fn info_lists_the_three_kinds_or_says_why_not() {
    let i = info(preset("sybase"), &mut server(true), &ctx()).unwrap();
    let keys: Vec<&str> = i.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "jobdef", "schedule"]);
    assert!(!i.logs);
    let mut f = Fake::default().rows("db_id('sybmgmtdb')", &["DBID"], vec![vec![None]]);
    let i = info(preset("sybase"), &mut f, &ctx()).unwrap();
    assert!(i.kinds.is_empty() && i.notes[0].contains("sybmgmtdb"));
    let l = items(preset("sybase"), &mut server(true), &ctx(), "schedule").unwrap();
    assert_eq!(l.items.len(), 2);
    assert_eq!(l.items[0].name, "daily");
    assert!(l.items[0]
        .description
        .as_deref()
        .unwrap()
        .starts_with("Cada 2 horas, desde las 10:00 hasta las 16:00 (Lunes, Viernes)"));
    let l = items(preset("sybase"), &mut server(true), &ctx(), "jobdef").unwrap();
    assert_eq!(l.items[0].id, "3");
    for k in ["job", "jobdef", "schedule"] {
        well_formed(&form(preset("sybase"), &mut server(true), &ctx(), k, None).unwrap());
    }
}

#[test]
fn the_engines_values_are_read_in_their_shapes() {
    assert_eq!(e::day_time(Some("11:00PM")), Some(23 * 3600));
    assert_eq!(e::day_time(Some("12:30AM")), Some(30 * 60));
    assert_eq!(
        e::day_time(Some("1900-01-01 08:15:00.000")),
        Some(8 * 3600 + 15 * 60)
    );
    assert_eq!(e::parse_repeats(Some("2hours"), None), (2, "hours".into()));
    assert_eq!(
        e::parse_repeats(Some("10"), Some("mm")),
        (10, "minutes".into())
    );
    assert_eq!(e::parse_repeats(Some("1dd"), None), (1, "days".into()));
    assert_eq!(e::parse_repeats(None, None), (0, "minutes".into()));
    assert_eq!(
        e::parse_days(Some("mon:WED, fri, xx")),
        ["Monday", "Wednesday", "Friday"]
    );
    // Day names in other scripts are skipped, not cut inside a character.
    assert_eq!(
        e::parse_days(Some("miércoles:Среда:土曜日:sábado, Sun")),
        ["Sunday"]
    );
    assert_eq!(e::parse_dates(Some("32:1, 15,40")), [1, 15, 32]);
}

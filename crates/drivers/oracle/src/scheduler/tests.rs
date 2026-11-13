//! Unit tests of the Scheduler designer: ids, names, literals, the blocks
//! every kind writes (create, each change, delete), the calendar, the
//! schedule's modes, validation (no server). One test at the end runs the
//! `DBMS_JOB` blocks against a server, when `DBINE_TEST_ORACLE_ADMIN_URL`
//! is set (`--ignored`).

use super::calendar::Calendar;
use super::chain::{self, ChainSpec, Rule, Step};
use super::choices::Catalog;
use super::class::{self, ClassSpec};
use super::credential::{self, CredentialSpec};
use super::destination::{self, GroupSpec as DestGroup, Member};
use super::global;
use super::job::{self, legacy, Action, Arg, JobSpec, Notification};
use super::program::{self, ProgArg, ProgramSpec};
use super::read::{select_list, Rec};
use super::resource::{self, IncompatSpec, ResourceSpec};
use super::schedule::{self, ScheduleSpec};
use super::script::{lit, Block, A};
use super::timing::{self, local_time, tz_ok, Ts, When};
use super::watcher::{self, WatcherSpec};
use super::window::{self, GroupSpec as WinGroup, WindowSpec};
use super::*;
use crate::jobs::{Access, Off, Views};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};

fn ctx() -> Ctx {
    Ctx {
        v: Some(Views::Dba),
        access: Access {
            user: "APP".into(),
            any_job: false,
            granted: HashSet::new(),
            maintained: ["SYS", "XDB"].iter().map(|s| s.to_string()).collect(),
        },
        privs: [
            "CREATE JOB",
            "MANAGE SCHEDULER",
            "CREATE CREDENTIAL",
            "CREATE RULE SET",
            "CREATE RULE",
            "CREATE EVALUATION CONTEXT",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        major: 23,
        minor: 0,
        cloud: false,
        off: Off::default(),
        globals: HashMap::from([
            ("DEFAULT_TIMEZONE".to_string(), "UTC".to_string()),
            ("EMAIL_SERVER".to_string(), String::new()),
            ("LOG_HISTORY".to_string(), "30".to_string()),
        ]),
        views: [
            "ALL_SCHEDULER_FILE_WATCHERS",
            "ALL_SCHEDULER_DB_DESTS",
            "ALL_SCHEDULER_EXTERNAL_DESTS",
            "ALL_SCHEDULER_GROUPS",
            "ALL_SCHEDULER_RESOURCES",
            "ALL_SCHEDULER_INCOMPATS",
            "ALL_CREDENTIALS",
            "ALL_SCHEDULER_CREDENTIALS",
            "ALL_SCHEDULER_NOTIFICATIONS",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    }
}

fn catalog() -> Catalog {
    let mut c = Catalog::default();
    let mut put = |k: &str, values: &[&str]| {
        c.lists.insert(
            k.into(),
            values
                .iter()
                .map(|v| (v.to_string(), v.to_string()))
                .collect(),
        );
    };
    put("schema", &["APP"]);
    put("job_class", &["SYS.DEFAULT_JOB_CLASS", "SYS.NIGHT"]);
    put("program", &["APP.P1", "APP.P2"]);
    put("chain", &["APP.C1"]);
    put("schedule", &["APP.S1"]);
    put("event_schedule", &["APP.EVT"]);
    put("window", &["SYS.W1", "SYS.W2"]);
    put("window_group", &["SYS.G1"]);
    put("file_watcher", &["APP.FW"]);
    put("credential", &["APP.CRED", "APP.CRED2"]);
    put("destination", &["APP.DB1", "SYS.AGENT1"]);
    put("external_destination", &["SYS.AGENT1"]);
    put("member:db", &["LOCAL", "APP.DB1"]);
    put("member:external", &["LOCAL", "SYS.AGENT1"]);
    put("job_or_program", &["APP.J1", "APP.J2", "APP.P1"]);
    put("resource_plan", &["DEFAULT_PLAN"]);
    put("consumer_group", &["BATCH_GROUP"]);
    put("service", &["SVC"]);
    c
}

fn map(v: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    serde_json::from_value(v).unwrap()
}

fn fv(values: serde_json::Value, lists: serde_json::Value) -> FormValues {
    FormValues {
        values: map(values),
        lists: serde_json::from_value(lists).unwrap(),
    }
}

/// A block the editor runs whole: one PL/SQL statement, ending in `/`.
fn one_block(script: &str) {
    let stmts = crate::script::split(script);
    assert_eq!(stmts.len(), 1, "{script}\n{stmts:#?}");
    assert!(stmts[0].plsql, "{script}");
    assert!(script.starts_with("-- "), "{script}");
    assert!(script.ends_with("END;\n/\n"), "{script}");
}

fn all_cols() -> HashSet<String> {
    [
        "JOB_PRIORITY",
        "LOGGING_LEVEL",
        "RESTARTABLE",
        "RESTART_ON_RECOVERY",
        "RESTART_ON_FAILURE",
        "MAX_RUNS",
        "MAX_FAILURES",
        "MAX_RUN_DURATION",
        "SCHEDULE_LIMIT",
        "INSTANCE_ID",
        "INSTANCE_STICKINESS",
        "STOP_ON_WINDOW_CLOSE",
        "RAISE_EVENTS",
        "STORE_OUTPUT",
        "ALLOW_RUNS_IN_RESTRICTED_MODE",
        "FAIL_ON_SCRIPT_ERROR",
        "CONNECT_CREDENTIAL_NAME",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ---------------------------------------------------------------------------
// Kinds, ids, names
// ---------------------------------------------------------------------------

#[test]
fn kinds_parse_and_unknown_ones_are_refused() {
    assert_eq!(Kind::parse(" job ").unwrap(), Kind::Job);
    assert_eq!(Kind::parse("window_group").unwrap(), Kind::WindowGroup);
    assert_eq!(Kind::parse("global").unwrap(), Kind::Global);
    for bad in ["", "jobs", "JOB", "operator", "job'; --"] {
        let e = Kind::parse(bad).unwrap_err();
        assert!(
            e.to_string().contains("no es un tipo de objeto"),
            "{bad}: {e}"
        );
    }
}

#[test]
fn ids_are_only_the_shapes_the_lists_give() {
    assert_eq!(
        parse_obj("APP.J1", "job").unwrap(),
        ("APP".into(), "J1".into())
    );
    assert_eq!(
        parse_obj("\"App\".\"j 1\"", "job").unwrap(),
        ("App".into(), "j 1".into())
    );
    assert_eq!(obj_id(Some("App"), "j 1"), "\"App\".\"j 1\"");
    assert_eq!(obj_id(None, "W1"), "SYS.W1");
    for bad in [
        "",
        "J1",
        "app.j1",
        "APP.J1'; DROP TABLE T; --",
        "\"APP\".\"J,1\"",
        "\"APP\".\"J\"\"1\"",
        "\"APP\".\"J1",
        "APP.J1.X",
        "DBMS_JOB:5",
        "\"APP\".\"J1\"",
    ] {
        let e = parse_obj(bad, "job").unwrap_err();
        assert!(e.to_string().contains("no es un id de job"), "{bad}: {e}");
    }
    // The refused text comes back short and on one line.
    let e = parse_obj(&format!("{}\nDROP", "A".repeat(300)), "job").unwrap_err();
    let m = e.to_string();
    assert!(!m.contains('\n') && m.len() < 200, "{m}");
}

#[test]
fn new_names_fold_like_oracle_and_refuse_what_calls_cant_take() {
    let c = ctx();
    assert_eq!(new_name(" mi_job ", &c, "Nombre").unwrap(), "MI_JOB");
    assert_eq!(new_name("Job$1#", &c, "Nombre").unwrap(), "JOB$1#");
    assert_eq!(new_name("mi job", &c, "Nombre").unwrap(), "mi job");
    assert_eq!(new_name("año", &c, "Nombre").unwrap(), "año");
    for (bad, why) in [
        ("", "Falta"),
        ("a\"b", "comillas"),
        ("a,b", "comas"),
        ("a\nb", "control"),
    ] {
        let e = new_name(bad, &c, "Nombre").unwrap_err();
        assert!(e.to_string().contains(why), "{bad}: {e}");
    }
    assert!(new_name(&"A".repeat(128), &c, "Nombre").is_ok());
    assert!(new_name(&"A".repeat(129), &c, "Nombre").is_err());
    let old = Ctx {
        major: 12,
        minor: 1,
        ..ctx()
    };
    assert!(new_name(&"A".repeat(31), &old, "Nombre")
        .unwrap_err()
        .to_string()
        .contains("30 bytes"));
}

#[test]
fn owners_are_the_login_unless_it_may_create_anywhere() {
    let c = ctx();
    let cat = catalog();
    let v = map(json!({"owner": "OTHER"}));
    let e = new_owner(Vals(&v), &c, Kind::Job, &cat).unwrap_err();
    assert!(e.to_string().contains("CREATE ANY JOB"), "{e}");
    let v = map(json!({}));
    assert_eq!(new_owner(Vals(&v), &c, Kind::Job, &cat).unwrap(), "APP");
    let mut any = ctx();
    any.privs.insert("CREATE ANY JOB".into());
    let v = map(json!({"owner": "NOPE"}));
    assert!(new_owner(Vals(&v), &any, Kind::Job, &cat).is_err());
    let mut cat2 = cat.clone();
    cat2.lists
        .get_mut("schema")
        .unwrap()
        .push(("OTHER".into(), "OTHER".into()));
    let v = map(json!({"owner": "other"}));
    assert_eq!(
        new_owner(Vals(&v), &any, Kind::Job, &cat2).unwrap(),
        "OTHER"
    );
    // Without CREATE JOB, not even in its own schema.
    let mut none = ctx();
    none.privs.clear();
    let v = map(json!({}));
    assert!(new_owner(Vals(&v), &none, Kind::Job, &cat).is_err());
}

#[test]
fn info_says_what_the_login_lacks() {
    let mut c = ctx();
    let info = info_of(&c);
    assert_eq!(info.kinds[0].key, "job");
    assert_eq!(info.kinds.len(), 15);
    assert!(info.notes.is_empty(), "{:?}", info.notes);
    c.privs.clear();
    c.v = Some(Views::All);
    let info = info_of(&c);
    let window = info.kinds.iter().find(|k| k.key == "window").unwrap();
    assert!(!window.can_create && !window.can_edit && !window.can_delete);
    let job = info.kinds.iter().find(|k| k.key == "job").unwrap();
    assert!(!job.can_create && job.can_edit);
    assert!(info.notes.iter().any(|n| n.contains("CREATE JOB")));
    assert!(info.notes.iter().any(|n| n.contains("MANAGE SCHEDULER")));
    assert!(info.notes.iter().any(|n| n.contains("DBA_")));
    // Autonomous and older versions.
    let cloud = Ctx {
        cloud: true,
        major: 19,
        ..ctx()
    };
    let info = info_of(&cloud);
    assert!(!info
        .kinds
        .iter()
        .any(|k| k.key == "file_watcher" || k.key.contains("destination")));
    assert!(info.notes.iter().any(|n| n.contains("Autonomous")));
    let old = Ctx { major: 18, ..ctx() };
    assert!(info_of(&old).notes.iter().any(|n| n.contains("DBMS_JOB")));
}

#[test]
fn kinds_say_their_actions_and_the_jobs_flags_by_privilege() {
    fn kind(info: &SchedulerInfo, k: &str) -> SchedulerKind {
        info.kinds.iter().find(|x| x.key == k).unwrap().clone()
    }
    fn keys(k: &SchedulerKind) -> Vec<&str> {
        k.actions.iter().map(|a| a.key.as_str()).collect()
    }
    // MANAGE SCHEDULER and CREATE JOB.
    let info = info_of(&ctx());
    let job = kind(&info, "job");
    assert!(job.run_from_step && job.clear_history);
    assert_eq!(keys(&job), ["force_stop", "end_detached"]);
    assert_eq!(
        keys(&kind(&info, "window")),
        ["open_window", "close_window"]
    );
    assert_eq!(keys(&kind(&info, "program")), ["run"]);
    assert_eq!(keys(&kind(&info, "chain")), ["run"]);
    for k in [
        "schedule",
        "window_group",
        "job_class",
        "credential",
        "file_watcher",
        "resource",
        "global",
    ] {
        let k = kind(&info, k);
        assert!(k.actions.is_empty(), "{}", k.key);
        assert!(!k.run_from_step && !k.clear_history, "{}", k.key);
    }
    // Labels in Spanish, for the menus.
    assert_eq!(job.actions[0].label, "Forzar la detención");
    // Without either: a job still starts at a step, its history stays.
    let mut none = ctx();
    none.privs.clear();
    let info = info_of(&none);
    let job = kind(&info, "job");
    assert!(job.run_from_step && !job.clear_history);
    assert_eq!(keys(&job), ["end_detached"]);
    assert!(kind(&info, "window").actions.is_empty());
    assert!(kind(&info, "program").actions.is_empty());
    assert!(kind(&info, "chain").actions.is_empty());
}

#[test]
fn any_job_stops_by_force_with_manage_scheduler() {
    // STOP_JOB with force takes MANAGE SCHEDULER, whoever owns the job:
    // another user's (without CREATE ANY JOB), and Oracle's own.
    let c = ctx();
    assert!(action::force_stoppable(&c, "APP.J", false));
    assert!(!c.may_change(Kind::Job, "OTHER", "J", false));
    assert!(action::force_stoppable(&c, "OTHER.J", false));
    assert!(action::force_stoppable(&c, "SYS.PURGE_LOG", true));
    assert!(action::force_stoppable(&c, "SYS.ORA$AT_OS_OPT_SY_1", true));
    // Not without it, nor a DBMS_JOB job, nor a name DBMS_SCHEDULER can't
    // take.
    let mut none = ctx();
    none.privs.clear();
    assert!(!action::force_stoppable(&none, "APP.J", false));
    assert!(!action::force_stoppable(&c, "DBMS_JOB:42", false));
    assert!(!action::force_stoppable(&c, "\"APP\".\"A,B\"", false));
    // Autonomous Database locks Oracle's own jobs down.
    let cloud = Ctx {
        cloud: true,
        ..ctx()
    };
    assert!(!action::force_stoppable(&cloud, "SYS.PURGE_LOG", true));
    assert!(action::force_stoppable(&cloud, "ADMIN.J", false));
}

#[test]
fn oracles_own_objects_are_locked_whoever_logs_in() {
    let c = ctx();
    // Its own; Oracle's (SYS or a schema Oracle maintains, SYSTEM aside);
    // another user's.
    assert_eq!(c.locked(Kind::Program, "APP", "P1", false), None);
    assert_eq!(
        c.locked(Kind::Program, "SYS", "P1", c.oracle_schema("SYS")),
        Some(ORACLE_OWN)
    );
    assert_eq!(
        c.locked(Kind::Schedule, "XDB", "S", c.oracle_schema("XDB")),
        Some(ORACLE_OWN)
    );
    assert!(!c.oracle_schema("SYSTEM"));
    assert_eq!(
        c.locked(Kind::Program, "OTHER", "P1", false),
        Some(OWNER_ONLY)
    );
    // Not even SYS changes SYS's.
    let sys = Ctx {
        access: Access {
            user: "SYS".into(),
            ..ctx().access
        },
        ..ctx()
    };
    assert_eq!(sys.locked(Kind::Job, "SYS", "J", true), Some(ORACLE_OWN));
    assert!(sys.may_change(Kind::Job, "SYS", "J", true));
    // Another user's: CREATE ANY JOB, or ALTER on it.
    let mut any = ctx();
    any.privs.insert("CREATE ANY JOB".into());
    assert_eq!(any.locked(Kind::Job, "OTHER", "J", false), None);
    let mut granted = ctx();
    granted.access.granted.insert(("OTHER".into(), "J".into()));
    assert_eq!(granted.locked(Kind::Job, "OTHER", "J", false), None);
    // SYS's kinds take MANAGE SCHEDULER.
    let mut none = ctx();
    none.privs.clear();
    assert_eq!(c.locked(Kind::Window, "SYS", "W1", false), None);
    assert_eq!(
        none.locked(Kind::Window, "SYS", "W1", false),
        Some(MANAGE_ONLY)
    );
    assert!(c
        .check_change(Kind::Schedule, "SYS", "S", true)
        .unwrap_err()
        .to_string()
        .contains("de Oracle"));
    assert_eq!(unless(true), Some(false));
    assert_eq!(unless(false), None);
    // Programs and chains to run: its own, EXECUTE on it, EXECUTE ANY
    // PROGRAM.
    let grants: HashSet<(String, String)> = HashSet::from([("OTHER".into(), "P".into())]);
    assert!(c.may_execute("APP", "P", &HashSet::new()));
    assert!(c.may_execute("OTHER", "P", &grants));
    assert!(!c.may_execute("OTHER", "Q", &grants));
    let mut exec = ctx();
    exec.privs.insert("EXECUTE ANY PROGRAM".into());
    assert!(exec.may_execute("OTHER", "Q", &HashSet::new()));
    // Oracle's maintenance windows and their group change (as Oracle
    // documents) but aren't dropped; its internal ones don't change.
    assert_eq!(window::edit_locked(&c, "MONDAY_WINDOW", true), None);
    assert_eq!(
        window::drop_locked(&c, "MONDAY_WINDOW", true),
        Some(ORACLE_NO_DROP)
    );
    assert_eq!(
        window::drop_locked(&c, "MAINTENANCE_WINDOW_GROUP", true),
        Some(ORACLE_NO_DROP)
    );
    assert_eq!(
        window::edit_locked(&c, "ORA$AT_WGRP_OS", true),
        Some(ORACLE_OWN)
    );
    // Its delete says the same as its form: not changed either.
    assert_eq!(
        window::drop_locked(&c, "ORA$AT_WGRP_OS", true),
        Some(ORACLE_OWN)
    );
    assert_eq!(window::edit_locked(&c, "MY_WINDOW", false), None);
    assert_eq!(window::drop_locked(&c, "MY_WINDOW", false), None);
    assert_eq!(
        window::edit_locked(&none, "MY_WINDOW", false),
        Some(MANAGE_ONLY)
    );
    assert_eq!(
        window::drop_locked(&none, "MONDAY_WINDOW", true),
        Some(MANAGE_ONLY)
    );
    // Which are Oracle's: the dictionary's flag, else the names it gives.
    assert!(installed_by(&None, "MONDAY_WINDOW", |n| n.ends_with("DAY_WINDOW")));
    let set = Some(HashSet::from(["MONDAY_WINDOW".to_string()]));
    assert!(installed_by(&set, "MONDAY_WINDOW", |_| false));
    assert!(!installed_by(&set, "MY_DAY_WINDOW", |_| true));
}

#[test]
fn windows_open_while_closed_and_close_while_open() {
    let open = Rec::of(&[("ACTIVE", "TRUE"), ("ENABLED", "TRUE")]);
    assert_eq!(window::window_actions(&open), ["close_window"]);
    let closed = Rec::of(&[("ACTIVE", "FALSE"), ("ENABLED", "TRUE")]);
    assert_eq!(window::window_actions(&closed), ["open_window"]);
    // Oracle doesn't open a disabled window (ORA-27490).
    let disabled = Rec::of(&[("ACTIVE", "FALSE"), ("ENABLED", "FALSE")]);
    assert!(window::window_actions(&disabled).is_empty());
}

#[test]
fn a_programs_run_is_named_as_oracle_names_a_chains() {
    assert_eq!(
        action::run_prefix("DBINE_TEST_SCHED_PROG2"),
        "RUN_PROG$DBINE_TE"
    );
    assert_eq!(action::run_prefix("P1"), "RUN_PROG$P1");
    // Only the plain head: GENERATE_JOB_NAME takes an identifier.
    assert_eq!(action::run_prefix("AB c"), "RUN_PROG$AB");
    assert_eq!(action::run_prefix("mi prog"), "RUN_PROG$");
    assert!(action::run_prefix(&"X".repeat(128)).len() <= 18);
}

#[test]
fn calendar_lists_the_frequency_doesnt_take_are_hidden_and_dropped() {
    let fields = timing::fields(&["calendar", "expression"], "UTC");
    let f = |k: &str| fields.iter().find(|f| f.key == k).unwrap().clone();
    let cal = Some(("schedule_mode".to_string(), vec!["calendar".to_string()]));
    assert_eq!(f("byweekno").when, cal);
    assert_eq!(
        f("byweekno").when_all,
        [("freq".to_string(), vec!["YEARLY".to_string()])]
    );
    assert_eq!(
        f("bysetpos").when_all,
        [(
            "freq".to_string(),
            vec!["YEARLY".to_string(), "MONTHLY".to_string()]
        )]
    );
    assert!(f("byday").when_all.is_empty() && f("byyearday").when_all.is_empty());
    // What they still hold doesn't reach the expression.
    let v = map(json!({"freq": "DAILY", "byweekno": "10", "bysetpos": "-1", "byhour": "3"}));
    assert_eq!(
        Calendar::from_values(Vals(&v)).unwrap().expression(),
        "FREQ=DAILY;BYHOUR=3"
    );
    let v = map(json!({"freq": "MONTHLY", "byweekno": "10", "bysetpos": "-1", "byday": ["MON"]}));
    assert_eq!(
        Calendar::from_values(Vals(&v)).unwrap().expression(),
        "FREQ=MONTHLY;BYDAY=MON;BYSETPOS=-1"
    );
    let v = map(json!({"freq": "YEARLY", "byweekno": "10"}));
    assert_eq!(
        Calendar::from_values(Vals(&v)).unwrap().expression(),
        "FREQ=YEARLY;BYWEEKNO=10"
    );
    // Nor does a hidden one's bad text.
    let v = map(json!({"freq": "DAILY", "byweekno": "x"}));
    assert!(Calendar::from_values(Vals(&v)).is_ok());
}

// ---------------------------------------------------------------------------
// Literals and blocks
// ---------------------------------------------------------------------------

#[test]
fn literals_double_quotes_and_never_end_the_block() {
    assert_eq!(lit("it's"), "'it''s'");
    assert_eq!(lit("a\nb"), "'a\nb'");
    let tricky = lit("BEGIN\n  NULL;\n/\n.\nEND;");
    assert!(
        !tricky.lines().any(|l| matches!(l.trim(), "/" | ".")),
        "{tricky}"
    );
    assert_eq!(
        tricky,
        "('BEGIN' || CHR(10) || '  NULL;' || CHR(10) || '/' || CHR(10) || '.' || CHR(10) || 'END;')"
    );
    let mut b = Block::new("x");
    b.call(
        "DBMS_SCHEDULER.X",
        vec![("a", A::S("BEGIN\n/\nEND;".into()))],
    );
    one_block(&b.script());
}

#[test]
fn secrets_are_masked_in_the_script_and_bound_in_the_block() {
    let mut b = Block::new("Crea la credencial APP.C.");
    b.call(
        "DBMS_CREDENTIAL.CREATE_CREDENTIAL",
        vec![
            ("credential_name", A::Name("APP.C".into())),
            ("username", A::S("u".into())),
            ("password", A::Secret("p'w :1".into())),
        ],
    );
    let script = b.script();
    assert!(
        script.contains(&format!("password => '{}'", dbine_driver::SECRET_MASK)),
        "{script}"
    );
    assert!(
        !script.contains("p'w") && !script.contains("p''w"),
        "{script}"
    );
    one_block(&script);
    let (sql, binds) = b.runnable();
    assert!(sql.contains("password => :1"), "{sql}");
    assert!(
        !sql.contains("p'w") && !sql.starts_with("--") && !sql.ends_with('/'),
        "{sql}"
    );
    assert_eq!(binds, vec!["p'w :1".to_string()]);
}

#[test]
fn a_create_undoes_itself_and_a_failed_edit_leaves_it_disabled() {
    let mut b = Block::new("c");
    b.pcall("DBMS_SCHEDULER.CREATE_X", vec![A::Name("APP.X".into())]);
    b.created("DBMS_SCHEDULER.DROP_X('\"APP\".\"X\"');");
    b.pcall("DBMS_SCHEDULER.ENABLE", vec![A::Name("APP.X".into())]);
    let s = b.script();
    assert!(
        s.contains("DECLARE\n  created BOOLEAN := FALSE;\nBEGIN\n"),
        "{s}"
    );
    assert!(s.contains("  created := TRUE;\n"), "{s}");
    assert!(
        s.contains("    IF created THEN\n      BEGIN\n        DBMS_SCHEDULER.DROP_X"),
        "{s}"
    );
    assert!(s.contains("    RAISE;\nEND;\n/\n"), "{s}");
    // The create itself comes before the flag: an object that already
    // existed is never dropped.
    assert!(s.find("CREATE_X").unwrap() < s.find("created := TRUE").unwrap());
    one_block(&s);

    assert!(
        !s.contains("confirma cada llamada"),
        "a create is all or nothing: {s}"
    );

    let mut b = Block::new("e");
    b.disabled_first("APP.X", |b| {
        b.pcall("DBMS_SCHEDULER.DISABLE", vec![A::Name("APP.X".into())])
    });
    b.set_attr(&A::Name("APP.X".into()), "job_action", A::S("y".into()));
    b.enabled_again(|b| b.pcall("DBMS_SCHEDULER.ENABLE", vec![A::Name("APP.X".into())]));
    let s = b.script();
    assert!(
        s.contains("disabled := TRUE;") && s.contains("disabled := FALSE;"),
        "{s}"
    );
    // A failure after the disable never enables it again: the calls before
    // are committed, and it would run half changed. The error says it
    // stayed disabled, the server's under it.
    let handler = &s[s.find("EXCEPTION").unwrap()..];
    assert!(!handler.contains("ENABLE"), "{handler}");
    assert!(
        handler.contains("    IF disabled THEN\n      RAISE_APPLICATION_ERROR(-20991, 'APP.X quedó deshabilitado: revisalo y habilitalo.', TRUE);\n    END IF;\n    RAISE;\nEND;"),
        "{handler}"
    );
    assert!(
        s.starts_with("-- e\n-- ⚠ Oracle confirma cada llamada: si una falla, las anteriores ya quedaron hechas.\n-- ⚠ Si una llamada falla después de deshabilitarlo, queda deshabilitado"),
        "{s}"
    );
    one_block(&s);
}

#[test]
fn edits_of_several_calls_say_each_one_commits() {
    let t = A::Name("APP.X".into());
    // One call: nothing to say.
    let mut b = Block::new("x");
    b.set_attr(&t, "comments", A::S("c".into()));
    assert!(
        !b.script().contains("confirma cada llamada"),
        "{}",
        b.script()
    );
    // Several: said under the first line, whatever kind the block is for.
    b.set_attr(&t, "max_runs", A::I(3));
    assert!(
        b.script()
            .starts_with("-- x\n-- ⚠ Oracle confirma cada llamada: si una falla, las anteriores ya quedaron hechas.\nBEGIN\n"),
        "{}",
        b.script()
    );
    // Only in the script shown, not in the block that runs.
    assert!(b.runnable().0.starts_with("BEGIN\n"));
    // DBMS_JOB's calls are one transaction: nothing to say.
    let mut b = Block::new("x");
    b.rollback_on_error();
    b.call(
        "DBMS_JOB.WHAT",
        vec![("job", A::I(1)), ("what", A::S("x;".into()))],
    );
    b.call(
        "DBMS_JOB.INTERVAL",
        vec![("job", A::I(1)), ("interval", A::S("null".into()))],
    );
    assert!(!b.script().contains("confirma cada llamada"));
    // The kinds whose edits had no note: a credential, a class, a group.
    let old = CredentialSpec {
        owner: "APP".into(),
        name: "CRED".into(),
        username: "u".into(),
        password: None,
        windows_domain: None,
        database_role: None,
        comments: None,
        enabled: true,
    };
    let mut new = old.clone();
    new.username = "v".into();
    new.comments = Some("c".into());
    new.enabled = false;
    let b = credential::alter_block(&old, &new, &ctx());
    assert!(b.calls() > 1);
    assert!(
        b.script().contains("-- ⚠ Oracle confirma cada llamada"),
        "{}",
        b.script()
    );
}

// ---------------------------------------------------------------------------
// Warnings the user accepts before a script runs
// ---------------------------------------------------------------------------

/// The script's warnings as the app reads them (`scriptWarnings`): every
/// line whose text, after an optional comment marker, starts with "⚠",
/// without the mark.
fn warnings(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|l| {
            let t = l.trim_start();
            let t = ["--", "#", "//", "/*", "*"]
                .iter()
                .find_map(|m| t.strip_prefix(m))
                .unwrap_or(t);
            t.trim_start().strip_prefix('⚠')
        })
        .map(|t| t.trim().trim_end_matches("*/").trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

const NOT_ATOMIC: &str =
    "Oracle confirma cada llamada: si una falla, las anteriores ya quedaron hechas.";
const LEFT_DISABLED: &str = "Si una llamada falla después de deshabilitarlo, queda deshabilitado (no se vuelve a habilitar a medio cambiar): el error lo dice.";

#[test]
fn an_edit_warns_it_isnt_atomic_and_may_stay_disabled_only_when_it_is_so() {
    let cols = all_cols();
    let old = job_spec();
    // One call: nothing to accept.
    let mut new = old.clone();
    new.comments = Some("x".into());
    assert!(warnings(&job::alter_block(&old, &new, &cols, &[]).script()).is_empty());
    // Several calls on a job that stays enabled: each one commits.
    new.priority = 4;
    assert_eq!(
        warnings(&job::alter_block(&old, &new, &cols, &[]).script()),
        vec![NOT_ATOMIC]
    );
    // What an enabled job runs changes: disabled first, left so if a call
    // after that fails.
    let mut new = old.clone();
    new.action = Action::Inline {
        kind: "PLSQL_BLOCK".into(),
        action: "BEGIN NULL; NULL; END;".into(),
    };
    let s = job::alter_block(&old, &new, &cols, &[]).script();
    assert_eq!(warnings(&s), vec![NOT_ATOMIC, LEFT_DISABLED], "{s}");
    // The same on a disabled job: nothing is disabled first.
    let mut off = old.clone();
    off.enabled = false;
    let mut new = off.clone();
    new.action = Action::Inline {
        kind: "PLSQL_BLOCK".into(),
        action: "BEGIN NULL; NULL; END;".into(),
    };
    new.comments = Some("x".into());
    assert_eq!(
        warnings(&job::alter_block(&off, &new, &cols, &[]).script()),
        vec![NOT_ATOMIC]
    );
    // Programs alike.
    let p = prog();
    let mut p2 = p.clone();
    p2.action = "APP.PROC2".into();
    assert_eq!(
        warnings(&program::alter_block(&p, &p2).script()),
        vec![NOT_ATOMIC, LEFT_DISABLED]
    );
    let mut p3 = p.clone();
    p3.comments = Some("c".into());
    assert!(warnings(&program::alter_block(&p, &p3).script()).is_empty());
    // Disabled in the same save: disabled first, but it ends disabled
    // anyway, so a failure risks nothing more than the other calls.
    let mut ends_off = old.clone();
    ends_off.action = Action::Inline {
        kind: "PLSQL_BLOCK".into(),
        action: "BEGIN NULL; NULL; END;".into(),
    };
    ends_off.enabled = false;
    let s = job::alter_block(&old, &ends_off, &cols, &[]).script();
    assert_eq!(warnings(&s), vec![NOT_ATOMIC], "{s}");
    assert!(
        s.contains("DBMS_SCHEDULER.DISABLE(name => '\"APP\".\"J1\"', force => TRUE);")
            && !s.contains("disabled := TRUE")
            && !s.contains("RAISE_APPLICATION_ERROR"),
        "{s}"
    );
    assert!(
        s.find("DBMS_SCHEDULER.DISABLE").unwrap() < s.find("'job_action'").unwrap(),
        "{s}"
    );
    assert_eq!(s.matches("DBMS_SCHEDULER.DISABLE").count(), 1, "{s}");
    one_block(&s);
    let mut p4 = p2.clone();
    p4.enabled = false;
    let s = program::alter_block(&p, &p4).script();
    assert_eq!(warnings(&s), vec![NOT_ATOMIC], "{s}");
    assert!(!s.contains("disabled := TRUE"), "{s}");
    assert_eq!(s.matches("DBMS_SCHEDULER.DISABLE").count(), 1, "{s}");
    one_block(&s);
    // A create drops what it made on a failure: all or nothing.
    assert!(warnings(&job::create_block(&old, &cols).script()).is_empty());
    assert!(warnings(&program::create_block(&p).script()).is_empty());
    // Only in the script shown: the block that runs has no comments.
    let mut on_new = ends_off.clone();
    on_new.enabled = true;
    assert!(job::alter_block(&old, &on_new, &cols, &[])
        .runnable()
        .0
        .starts_with("DECLARE\n"));
    assert!(job::alter_block(&old, &ends_off, &cols, &[])
        .runnable()
        .0
        .starts_with("BEGIN\n"));
}

#[test]
fn a_delete_warns_of_what_it_takes_along_only_when_there_is_some() {
    // A window: the window groups it's in lose it.
    assert!(warnings(&window::drop_block("W1", &[]).script()).is_empty());
    assert_eq!(
        warnings(&window::drop_block("W1", &["SYS.G1".into(), "SYS.G2".into()]).script()),
        vec!["La ventana sale también de los grupos de ventanas que la incluyen: SYS.G1, SYS.G2."]
    );
    // A long list ends in "…".
    let many: Vec<String> = (0..30).map(|i| format!("SYS.G{i}")).collect();
    let w = warnings(&window::drop_block("W1", &many).script());
    assert!(w[0].ends_with(": SYS.G0, SYS.G1, SYS.G2, SYS.G3, SYS.G4, SYS.G5, SYS.G6, SYS.G7, SYS.G8, SYS.G9, SYS.G10, SYS.G11, SYS.G12, SYS.G13, SYS.G14, SYS.G15, SYS.G16, SYS.G17, SYS.G18, SYS.G19, …."), "{w:?}");
    // A database destination: its groups, and its jobs' state there.
    assert!(warnings(&destination::db_drop_block("APP.DB1", &[], &[]).script()).is_empty());
    let s = destination::db_drop_block("APP.DB1", &["APP.DG".into()], &["APP.J1".into()]).script();
    assert_eq!(
        warnings(&s),
        vec![
            "El destino sale también de los grupos de destinos que lo incluyen: APP.DG.",
            "Los jobs que corren en este destino pierden allí su estado y sus contadores, y se detienen si se están ejecutando en él: APP.J1.",
        ]
    );
    one_block(&s);
    // An agent: it registers again to come back, always; what goes through
    // it, when there's some.
    const AGAIN: &str = "Para volver, el agente tiene que registrarse de nuevo en esta base (schagent -registerdatabase).";
    let s = destination::external_drop_block("AGENT1", &[], &[], &[]).script();
    assert_eq!(warnings(&s), vec![AGAIN]);
    assert!(
        s.starts_with("-- Quita el agente AGENT1 de los destinos de esta base.\n"),
        "{s}"
    );
    let s = destination::external_drop_block(
        "AGENT1",
        &["APP.DB1".into()],
        &["APP.DG".into()],
        &["APP.J1".into()],
    )
    .script();
    let w = warnings(&s);
    assert_eq!(w.len(), 4, "{w:#?}");
    assert_eq!(w[0], AGAIN);
    assert_eq!(
        w[1],
        "Los destinos de base de datos que usan este agente quedan deshabilitados y sin agente: APP.DB1."
    );
    one_block(&s);
    // A resource: the jobs and programs it's assigned to, when there are.
    let mut r = ResourceSpec {
        owner: "APP".into(),
        name: "R".into(),
        units: 4,
        status: "ENFORCE_CONSTRAINTS".into(),
        level: "JOB_LEVEL".into(),
        comments: None,
        constraints: Vec::new(),
    };
    let s = resource::resource_drop(&r).script();
    assert!(warnings(&s).is_empty(), "{s}");
    assert!(s.contains("force => TRUE"), "{s}");
    r.constraints = vec![("APP.J1".into(), 2), ("APP.P1".into(), 1)];
    assert_eq!(
        warnings(&resource::resource_drop(&r).script()),
        vec!["Los jobs y programas que lo usan dejan de tenerlo asignado: APP.J1, APP.P1."]
    );
}

#[test]
fn a_destination_group_warns_its_jobs_only_when_a_member_goes() {
    let g = DestGroup {
        owner: "APP".into(),
        name: "DG".into(),
        group_type: "DB_DEST".into(),
        members: vec![
            Member::parse("\"APP\".\"DB1\"", "SYS"),
            Member::parse("LOCAL", "SYS"),
        ],
        enabled: true,
        comments: None,
    };
    let users = vec!["APP.J1".to_string()];
    const GONE: &str = "Los jobs que usan este grupo pierden el estado y los contadores que tenían en los miembros que se quitan o cambian: APP.J1.";
    // A member added: nothing is lost.
    let mut add = g.clone();
    add.members.push(Member::parse("\"APP\".\"DB2\"", "SYS"));
    assert!(warnings(&destination::group_alter(&g, &add, &users).script()).is_empty());
    // One removed, or its credential changed, with jobs on the group.
    let mut rm = g.clone();
    rm.members.retain(|m| m.destination != "LOCAL");
    assert_eq!(
        warnings(&destination::group_alter(&g, &rm, &users).script()),
        vec![GONE]
    );
    let mut cred = g.clone();
    cred.members[0].credential = Some("APP.CRED".into());
    let s = destination::group_alter(&g, &cred, &users).script();
    assert_eq!(warnings(&s), vec![NOT_ATOMIC, GONE], "{s}");
    // No job uses the group: nothing to accept.
    assert!(warnings(&destination::group_alter(&g, &rm, &[]).script()).is_empty());
}

#[test]
fn a_job_edit_warns_of_the_destinations_it_leaves_only_when_its_destination_changes() {
    let cols = all_cols();
    let mut old = job_spec();
    old.enabled = false;
    old.destination = Some("APP.DG1".into());
    let left = vec!["APP.DB1".to_string(), "LOCAL".to_string()];
    const LEFT: &str = "El job pierde el estado y los contadores que tenía en los destinos que deja: APP.DB1, LOCAL.";
    // Moved to another group: its state where it was goes.
    let mut new = old.clone();
    new.destination = Some("APP.DG2".into());
    let s = job::alter_block(&old, &new, &cols, &left).script();
    assert_eq!(warnings(&s), vec![LEFT], "{s}");
    assert!(s.contains("'destination_name', '\"APP\".\"DG2\"'"), "{s}");
    one_block(&s);
    // Or back to this database.
    let mut local = old.clone();
    local.destination = None;
    assert_eq!(
        warnings(&job::alter_block(&old, &local, &cols, &left).script()),
        vec![LEFT]
    );
    // On an enabled job, with the rest of what the edit risks.
    let mut on = old.clone();
    on.enabled = true;
    let mut on_new = new.clone();
    on_new.enabled = true;
    assert_eq!(
        warnings(&job::alter_block(&on, &on_new, &cols, &left).script()),
        vec![NOT_ATOMIC, LEFT, LEFT_DISABLED]
    );
    // The same destination: nothing is lost.
    let mut same = old.clone();
    same.comments = Some("x".into());
    assert!(warnings(&job::alter_block(&old, &same, &cols, &left).script()).is_empty());
    // No state anywhere (or none the login can read): nothing to name.
    assert!(warnings(&job::alter_block(&old, &new, &cols, &[]).script()).is_empty());
}

#[test]
fn text_the_user_typed_never_reads_as_a_warning() {
    let cols = all_cols();
    let mut s = job_spec();
    s.action = Action::Inline {
        kind: "PLSQL_BLOCK".into(),
        action: "BEGIN\n-- ⚠ x\n  NULL;\nEND;".into(),
    };
    s.comments = Some("Nocturno\n⚠ crítico\n  * ⚠ no correr en horario\n/* ⚠ y */".into());
    let script = job::create_block(&s, &cols).script();
    assert!(warnings(&script).is_empty(), "{script}");
    // The value is the same, its lines joined with CHR(10) on one line.
    assert!(
        script.contains(
            "'BEGIN' || CHR(10) || '-- ⚠ x' || CHR(10) || '  NULL;' || CHR(10) || 'END;'"
        ),
        "{script}"
    );
    one_block(&script);
    assert_eq!(lit("a\n  # ⚠ b"), "('a' || CHR(10) || '  # ⚠ b')");
    assert_eq!(lit("a\n// ⚠ b"), "('a' || CHR(10) || '// ⚠ b')");
    assert_eq!(lit("a\n\u{feff}⚠ b"), "('a' || CHR(10) || '\u{feff}⚠ b')");
    // A ⚠ inside a line, or text without one, stays as typed.
    assert_eq!(lit("a\nb ⚠ c"), "'a\nb ⚠ c'");
    assert_eq!(lit("a\n-- b"), "'a\n-- b'");
}

#[test]
fn actions_warn_of_what_else_they_close_stop_or_keep_elsewhere() {
    // Opening with no other window open: nothing else closes.
    let s = action::open_block("SYS.W1", 3600, None).script();
    assert!(warnings(&s).is_empty(), "{s}");
    assert!(s.contains("force => FALSE"), "{s}");
    // Another one open: it closes, with the jobs that stop with it.
    const NO_OTHER: &str = "Mientras esté abierta, Oracle no abre ninguna otra ventana programada, aunque tenga más prioridad.";
    let closing = ("W2".to_string(), Vec::new());
    let s = action::open_block("SYS.W1", 3600, Some(&closing)).script();
    assert_eq!(
        warnings(&s),
        vec![
            "Solo puede haber una ventana abierta: cierra la que está abierta ahora, SYS.W2 (force => TRUE).",
            NO_OTHER,
        ]
    );
    assert!(s.contains("force => TRUE"), "{s}");
    const STOP: &str = "Se detienen ahora estos jobs, que se están ejecutando con «Detener al cerrar la ventana»: APP.J1.";
    let closing = ("W2".to_string(), vec!["APP.J1".to_string()]);
    let w = warnings(&action::open_block("SYS.W1", 3600, Some(&closing)).script());
    assert_eq!(w.len(), 3, "{w:#?}");
    assert_eq!(w[1], STOP);
    assert_eq!(w[2], NO_OTHER);
    // Closing: a warning only when running jobs stop.
    let s = action::close_block("SYS.W1", &[]).script();
    assert!(warnings(&s).is_empty(), "{s}");
    assert_eq!(
        warnings(&action::close_block("SYS.W1", &["APP.J1".into()]).script()),
        vec![STOP]
    );
    // A program or a chain run once: the run-once job Oracle drops when it
    // ends is only described, nothing the user has is lost.
    let s = action::run_program_block("APP.P1", "P1", "APP").script();
    assert!(warnings(&s).is_empty(), "{s}");
    one_block(&s);
    let s = action::run_chain_block("APP.C1", "APP").script();
    assert!(warnings(&s).is_empty(), "{s}");
    one_block(&s);
}

#[test]
fn only_chain_jobs_start_at_a_step_and_dbms_job_ones_keep_no_history() {
    let chains: HashSet<String> = HashSet::from(["APP.CJ".to_string()]);
    assert_eq!(job::starts_at_step("APP.CJ", Some(&chains)), Some(true));
    assert_eq!(job::starts_at_step("APP.J1", Some(&chains)), Some(false));
    assert_eq!(
        job::starts_at_step("DBMS_JOB:42", Some(&chains)),
        Some(false)
    );
    assert_eq!(job::starts_at_step("DBMS_JOB:42", None), Some(false));
    // The chain jobs couldn't be read: as the kind says.
    assert_eq!(job::starts_at_step("APP.J1", None), None);
    assert_eq!(job::keeps_history("DBMS_JOB:42"), Some(false));
    assert_eq!(job::keeps_history("APP.J1"), None);
}

#[test]
fn control_characters_in_values_never_reach_the_block_raw() {
    // U+0001 is where a secret waits in the body: a value with it is
    // written as CHR(1), never taken for a secret.
    assert_eq!(lit("a\u{1}b"), "('a' || CHR(1) || 'b')");
    assert_eq!(lit("\u{1}"), "(CHR(1))");
    assert_eq!(lit("a\tb\r"), "'a\tb\r'");
    assert_eq!(
        lit("x\u{0}\n/\n\u{1b}"),
        "('x' || CHR(0) || CHR(10) || '/' || CHR(10) || CHR(27))"
    );
    let mut b = Block::new("c\u{1}");
    b.call(
        "DBMS_CREDENTIAL.CREATE_CREDENTIAL",
        vec![
            ("comments", A::S("one\u{1}2\u{1}two".into())),
            ("password", A::Secret("pw".into())),
            ("username", A::S("u\u{1}".into())),
        ],
    );
    let s = b.script();
    assert!(
        s.contains("comments => ('one' || CHR(1) || '2' || CHR(1) || 'two'),\n      password => '********',\n      username => ('u' || CHR(1)));"),
        "{s}"
    );
    assert!(!s.contains('\u{1}'), "{s:?}");
    let (run, binds) = b.runnable();
    assert!(
        run.contains("comments => ('one' || CHR(1) || '2' || CHR(1) || 'two'),\n      password => :1,\n      username => ('u' || CHR(1)));"),
        "{run}"
    );
    assert_eq!(binds, vec!["pw".to_string()]);
    one_block(&s);
}

#[test]
fn null_attributes_are_set_null() {
    let mut b = Block::new("x");
    let t = A::Name("APP.J".into());
    b.set_attr(&t, "max_runs", A::Null);
    b.set_attr(&t, "max_runs", A::I(3));
    b.set_attr2(&t, "event_spec", A::Null, A::Null);
    b.set_attr2(&t, "event_spec", A::Null, A::Name("APP.FW".into()));
    let s = b.script();
    assert!(
        s.contains("SET_ATTRIBUTE_NULL('\"APP\".\"J\"', 'max_runs');"),
        "{s}"
    );
    assert!(
        s.contains("SET_ATTRIBUTE('\"APP\".\"J\"', 'max_runs', 3);"),
        "{s}"
    );
    assert!(
        s.contains("SET_ATTRIBUTE_NULL('\"APP\".\"J\"', 'event_spec');"),
        "{s}"
    );
    assert!(s.contains("'event_spec', NULL, '\"APP\".\"FW\"'"), "{s}");
    assert_eq!(b.calls(), 4);
}

#[test]
fn fingerprints_change_with_the_values() {
    let a = fingerprint(&map(json!({"a": 1})), &BTreeMap::new());
    assert_eq!(a, fingerprint(&map(json!({"a": 1})), &BTreeMap::new()));
    assert_ne!(a, fingerprint(&map(json!({"a": 2})), &BTreeMap::new()));
    let v = fv(json!({"dbine_state": "x"}), json!({}));
    assert!(fresh(&v, "x").is_ok());
    assert!(fresh(&v, "y").unwrap_err().to_string().contains("cambió"));
    assert!(fresh(&fv(json!({}), json!({})), "y").is_ok());
}

#[test]
fn views_are_read_with_their_times_as_text() {
    let sql = select_list(&[
        ("OWNER".into(), "DB_TYPE_VARCHAR".into()),
        ("START_DATE".into(), "DB_TYPE_TIMESTAMP_TZ".into()),
        ("MAX_RUN_DURATION".into(), "DB_TYPE_INTERVAL_DS".into()),
        ("ANYDATA_VALUE".into(), "DB_TYPE_OBJECT".into()),
        ("ERROR#".into(), "DB_TYPE_NUMBER".into()),
    ]);
    assert!(sql.starts_with("\"OWNER\", TO_CHAR(\"START_DATE\", 'YYYY-MM-DD\"T\"HH24:MI:SS') AS \"START_DATE\", TO_CHAR(\"START_DATE\", 'TZR') AS \"START_DATE_TZR\""), "{sql}");
    assert!(
        sql.contains("EXTRACT(DAY FROM \"MAX_RUN_DURATION\") * 86400"),
        "{sql}"
    );
    assert!(
        sql.contains("CASE WHEN \"ANYDATA_VALUE\" IS NULL THEN NULL ELSE 'Y' END"),
        "{sql}"
    );
    assert!(sql.ends_with("\"ERROR#\""), "{sql}");
}

// ---------------------------------------------------------------------------
// Calendar and schedules
// ---------------------------------------------------------------------------

#[test]
fn calendaring_expressions_the_fields_can_say_round_trip() {
    for e in [
        "FREQ=DAILY",
        "FREQ=WEEKLY;BYDAY=MON,WED;BYHOUR=3;BYMINUTE=30",
        "FREQ=MONTHLY;INTERVAL=2;BYMONTHDAY=1,15,-1;BYHOUR=0;BYMINUTE=0;BYSECOND=0",
        "FREQ=YEARLY;BYMONTH=JAN,JUL;BYWEEKNO=1;BYYEARDAY=-1;BYSETPOS=-1",
    ] {
        let c = Calendar::parse(e).unwrap_or_else(|| panic!("{e}"));
        assert_eq!(c.expression(), e);
    }
    // Spellings Oracle takes come back in one.
    let c = Calendar::parse(" freq = weekly ; byday = monday,fri ; bymonth = 3 ").unwrap();
    assert_eq!(c.expression(), "FREQ=WEEKLY;BYMONTH=MAR;BYDAY=MON,FRI");
    // What the fields can't say stays an expression.
    for e in [
        "FREQ=MONTHLY;BYDAY=-1FRI",
        "FREQ=DAILY;BYDATE=0101",
        "FREQ=MY_SCHEDULE",
        "FREQ=DAILY;EXCLUDE=HOLIDAYS",
        "SYSTIMESTAMP + INTERVAL '1' DAY",
        "FREQ=DAILY;BYHOUR=24",
        "FREQ=DAILY;BYMONTHDAY=0",
        "FREQ=DAILY;FREQ=HOURLY",
        "BYHOUR=3",
        "",
    ] {
        assert!(Calendar::parse(e).is_none(), "{e}");
    }
}

#[test]
fn calendar_fields_are_checked() {
    let ok = map(json!({"freq": "hourly", "interval": "4", "byminute": "0, 30"}));
    let c = Calendar::from_values(Vals(&ok)).unwrap();
    assert_eq!(c.expression(), "FREQ=HOURLY;INTERVAL=4;BYMINUTE=0,30");
    for (bad, why) in [
        (json!({"freq": "SOMETIMES"}), "Frecuencia"),
        (json!({"freq": "DAILY", "interval": 0}), "Cada"),
        (json!({"freq": "DAILY", "byhour": "25"}), "Horas"),
        (json!({"freq": "DAILY", "bymonthday": "0"}), "Días del mes"),
        (json!({"freq": "DAILY", "byminute": "1;DROP"}), "Minutos"),
        (
            json!({"freq": "DAILY", "byday": ["XYZ"]}),
            "Días de la semana",
        ),
        (json!({"freq": "DAILY", "bymonth": ["13"]}), "Meses"),
    ] {
        let v = map(bad);
        let e = Calendar::from_values(Vals(&v)).unwrap_err();
        assert!(e.to_string().contains(why), "{e}");
    }
}

#[test]
fn times_and_zones_are_checked() {
    assert_eq!(
        local_time("2030-01-02T03:04:05").unwrap(),
        "2030-01-02T03:04:05"
    );
    assert_eq!(
        local_time("2030-01-02 03:04").unwrap(),
        "2030-01-02T03:04:00"
    );
    assert_eq!(
        local_time("2030-01-02T03:04:05-03:00").unwrap(),
        "2030-01-02T03:04:05"
    );
    assert_eq!(
        local_time("2030-01-02T03:04:05.123Z").unwrap(),
        "2030-01-02T03:04:05"
    );
    for bad in [
        "2030-02-30T00:00:00",
        "2030-01-02",
        "2030-01-02T24:00:00",
        "x",
        "2030-01-02T3:4:5",
    ] {
        assert!(local_time(bad).is_none(), "{bad}");
    }
    for ok in [
        "UTC",
        "America/Argentina/Buenos_Aires",
        "PST8PDT",
        "-03:00",
        "+05:30",
        "Etc/GMT+3",
    ] {
        assert!(tz_ok(ok), "{ok}");
    }
    for bad in ["", "-3:00", "+25:00", "UTC'); DROP", "a b", "1UTC"] {
        assert!(!tz_ok(bad), "{bad}");
    }
    let t = Ts {
        local: "2030-01-02T03:04:05".into(),
        tz: "UTC".into(),
    };
    assert_eq!(
        t.expr(),
        "TO_TIMESTAMP_TZ('2030-01-02 03:04:05 UTC', 'YYYY-MM-DD HH24:MI:SS TZR')"
    );
    let t = Ts {
        tz: String::new(),
        ..t
    };
    assert_eq!(
        t.expr(),
        "TO_TIMESTAMP_TZ('2030-01-02 03:04:05', 'YYYY-MM-DD HH24:MI:SS')"
    );
}

fn ts(local: &str) -> Ts {
    Ts {
        local: local.into(),
        tz: "UTC".into(),
    }
}

#[test]
fn schedules_are_read_in_every_mode() {
    let r = Rec::of(&[
        ("REPEAT_INTERVAL", "FREQ=DAILY;BYHOUR=2"),
        ("START_DATE", "2030-01-01T00:00:00"),
        ("START_DATE_TZR", "utc"),
    ]);
    assert_eq!(
        When::of(&r),
        When::Calendar {
            start: Some(ts("2030-01-01T00:00:00")),
            cal: Calendar::parse("FREQ=DAILY;BYHOUR=2").unwrap(),
            end: None
        }
    );
    let r = Rec::of(&[("REPEAT_INTERVAL", "SYSDATE + 1")]);
    assert!(matches!(When::of(&r), When::Expression { .. }));
    let r = Rec::of(&[
        ("SCHEDULE_OWNER", "SYS"),
        ("SCHEDULE_NAME", "W1"),
        ("SCHEDULE_TYPE", "WINDOW"),
    ]);
    assert_eq!(When::of(&r), When::Window("SYS.W1".into()));
    let r = Rec::of(&[
        ("SCHEDULE_OWNER", "SYS"),
        ("SCHEDULE_NAME", "G1"),
        ("SCHEDULE_TYPE", "WINDOW_GROUP"),
    ]);
    assert_eq!(When::of(&r), When::WindowGroup("SYS.G1".into()));
    let r = Rec::of(&[
        ("SCHEDULE_OWNER", "APP"),
        ("SCHEDULE_NAME", "S1"),
        ("SCHEDULE_TYPE", "NAMED"),
    ]);
    assert_eq!(When::of(&r), When::Named("APP.S1".into()));
    let r = Rec::of(&[
        ("EVENT_QUEUE_OWNER", "APP"),
        ("EVENT_QUEUE_NAME", "Q"),
        ("EVENT_QUEUE_AGENT", "AG"),
        ("EVENT_CONDITION", "tab.user_data.x = 1"),
    ]);
    assert_eq!(
        When::of(&r),
        When::Event {
            start: None,
            queue: "APP.Q, AG".into(),
            condition: Some("tab.user_data.x = 1".into()),
            end: None
        }
    );
    let r = Rec::of(&[
        ("FILE_WATCHER_OWNER", "APP"),
        ("FILE_WATCHER_NAME", "FW"),
        ("EVENT_QUEUE_NAME", "SCHEDULER_FILEWATCHER_Q"),
    ]);
    assert_eq!(When::of(&r), When::FileWatcher("APP.FW".into()));
    let r = Rec::of(&[
        ("START_DATE", "2030-01-01T00:00:00"),
        ("START_DATE_TZR", "-03:00"),
    ]);
    assert_eq!(
        When::of(&r),
        When::Once(Ts {
            local: "2030-01-01T00:00:00".into(),
            tz: "-03:00".into()
        })
    );
    assert_eq!(When::of(&Rec::default()), When::Immediate);
}

#[test]
fn schedule_values_round_trip() {
    let whens = [
        When::Immediate,
        When::Once(ts("2030-01-01T00:00:00")),
        When::Calendar {
            start: Some(ts("2030-01-01T00:00:00")),
            cal: Calendar::parse("FREQ=WEEKLY;BYDAY=MON;BYHOUR=3").unwrap(),
            end: Some(ts("2031-01-01T00:00:00")),
        },
        When::Expression {
            start: None,
            expr: "FREQ=MONTHLY;BYDAY=-1FRI".into(),
            end: None,
        },
        When::Named("APP.S1".into()),
        When::Window("SYS.W1".into()),
        When::WindowGroup("SYS.G1".into()),
        When::Event {
            start: None,
            queue: "APP.Q".into(),
            condition: Some("1 = 1".into()),
            end: None,
        },
        When::FileWatcher("APP.FW".into()),
    ];
    let modes = [
        "immediate",
        "once",
        "calendar",
        "expression",
        "named",
        "window",
        "window_group",
        "event",
        "file_watcher",
    ];
    for w in whens {
        let mut v = BTreeMap::new();
        w.values(&mut |k: &str, x: serde_json::Value| {
            v.insert(k.to_string(), x);
        });
        assert_eq!(When::from_values(Vals(&v), &modes).unwrap(), w);
    }
    // A mode the kind doesn't have, an end before the start.
    let v = map(json!({"schedule_mode": "window", "window": "SYS.W1"}));
    assert!(When::from_values(Vals(&v), &["calendar"]).is_err());
    let v = map(
        json!({"schedule_mode": "calendar", "freq": "DAILY", "start_date": "2030-01-02T00:00:00", "end_date": "2030-01-01T00:00:00"}),
    );
    assert!(When::from_values(Vals(&v), &["calendar"])
        .unwrap_err()
        .to_string()
        .contains("posterior"));
    let v = map(json!({"schedule_mode": "named", "schedule": "x'; drop"}));
    assert!(When::from_values(Vals(&v), &["named"]).is_err());
    let v = map(json!({"schedule_mode": "once", "time_zone": "Mars/Base'"}));
    assert!(When::from_values(Vals(&v), &["once"])
        .unwrap_err()
        .to_string()
        .contains("Zona horaria"));
}

fn changes(old: &When, new: &When) -> String {
    let mut b = Block::new("x");
    timing::changes(&mut b, &A::Name("APP.J".into()), old, new);
    b.script()
}

#[test]
fn schedule_changes_null_the_old_mode_first() {
    let cal = When::Calendar {
        start: Some(ts("2030-01-01T00:00:00")),
        cal: Calendar::parse("FREQ=DAILY").unwrap(),
        end: None,
    };
    let s = changes(&cal, &When::Named("APP.S1".into()));
    let null_repeat = s
        .find("SET_ATTRIBUTE_NULL('\"APP\".\"J\"', 'repeat_interval')")
        .unwrap();
    let null_start = s
        .find("SET_ATTRIBUTE_NULL('\"APP\".\"J\"', 'start_date')")
        .unwrap();
    let named = s.find("'schedule_name', '\"APP\".\"S1\"'").unwrap();
    assert!(null_repeat < named && null_start < named, "{s}");
    let s = changes(&When::Window("SYS.W1".into()), &cal);
    assert!(
        s.find("SET_ATTRIBUTE_NULL('\"APP\".\"J\"', 'schedule_name')")
            .unwrap()
            < s.find("'repeat_interval', 'FREQ=DAILY'").unwrap(),
        "{s}"
    );
    assert!(
        s.contains("'start_date',") && s.contains("TO_TIMESTAMP_TZ('2030-01-01 00:00:00 UTC'"),
        "{s}"
    );
    // Same mode: only what differs.
    let cal2 = When::Calendar {
        start: Some(ts("2030-01-01T00:00:00")),
        cal: Calendar::parse("FREQ=DAILY;BYHOUR=4").unwrap(),
        end: None,
    };
    let s = changes(&cal, &cal2);
    assert!(
        s.contains("'repeat_interval', 'FREQ=DAILY;BYHOUR=4'") && !s.contains("start_date"),
        "{s}"
    );
    assert_eq!(changes(&cal, &cal), Block::new("x").script());
    let s = changes(
        &When::Named("APP.S1".into()),
        &When::Window("SYS.W1".into()),
    );
    assert!(
        s.contains("'schedule_name', '\"SYS\".\"W1\"'") && !s.contains("SET_ATTRIBUTE_NULL"),
        "{s}"
    );
    let s = changes(&When::Immediate, &When::FileWatcher("APP.FW".into()));
    assert!(s.contains("'event_spec', NULL, '\"APP\".\"FW\"'"), "{s}");
    let ev = When::Event {
        start: None,
        queue: "APP.Q".into(),
        condition: Some("a = 1".into()),
        end: None,
    };
    let s = changes(&cal, &ev);
    assert!(
        s.contains("'event_spec', 'a = 1', 'APP.Q'")
            && s.contains("SET_ATTRIBUTE_NULL('\"APP\".\"J\"', 'repeat_interval')"),
        "{s}"
    );
    let s = changes(&cal, &When::Immediate);
    assert!(
        s.contains("'repeat_interval')") && s.contains("'start_date')"),
        "{s}"
    );
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

fn job_spec() -> JobSpec {
    JobSpec {
        action: Action::Inline {
            kind: "PLSQL_BLOCK".into(),
            action: "BEGIN NULL; END;".into(),
        },
        when: When::Calendar {
            start: None,
            cal: Calendar::parse("FREQ=DAILY;BYHOUR=2").unwrap(),
            end: None,
        },
        ..JobSpec::defaults("APP", "J1")
    }
}

#[test]
fn a_new_job_is_created_disabled_given_the_rest_and_enabled() {
    let mut s = job_spec();
    s.comments = Some("it's".into());
    s.priority = 1;
    s.logging = "FULL".into();
    s.max_run_duration = Some(600);
    s.raise_events = vec!["JOB_STARTED".into(), "JOB_FAILED".into()];
    s.class = "SYS.NIGHT".into();
    s.notifications = vec![Notification {
        recipients: vec!["a@b.c".into()],
        events: vec!["JOB_FAILED".into()],
        sender: None,
        subject: Some("S".into()),
        body: None,
        filter: None,
    }];
    let script = job::create_block(&s, &all_cols()).script();
    one_block(&script);
    assert!(script.contains("DBMS_SCHEDULER.CREATE_JOB(\n      job_name => '\"APP\".\"J1\"',\n      job_type => 'PLSQL_BLOCK',\n      job_action => 'BEGIN NULL; END;',\n      repeat_interval => 'FREQ=DAILY;BYHOUR=2',\n      job_class => '\"NIGHT\"',\n      enabled => FALSE,\n      auto_drop => FALSE,\n      comments => 'it''s');"), "{script}");
    assert!(script.contains("'job_priority', 1"), "{script}");
    assert!(
        script.contains("'logging_level', DBMS_SCHEDULER.LOGGING_FULL"),
        "{script}"
    );
    assert!(
        script.contains("NUMTODSINTERVAL(600, 'SECOND')"),
        "{script}"
    );
    assert!(
        script.contains("DBMS_SCHEDULER.JOB_STARTED + DBMS_SCHEDULER.JOB_FAILED"),
        "{script}"
    );
    assert!(script.contains("ADD_JOB_EMAIL_NOTIFICATION"), "{script}");
    // Defaults aren't set again.
    assert!(
        !script.contains("store_output") && !script.contains("instance_stickiness"),
        "{script}"
    );
    assert!(
        script
            .trim_end()
            .contains("DBMS_SCHEDULER.ENABLE('\"APP\".\"J1\"');\nEXCEPTION"),
        "{script}"
    );
    // An attribute the version lacks isn't written.
    let mut cols = all_cols();
    cols.remove("JOB_PRIORITY");
    assert!(!job::create_block(&s, &cols)
        .script()
        .contains("job_priority"));
}

#[test]
fn program_jobs_take_their_style_and_arguments() {
    let mut s = job_spec();
    s.action = Action::Program("APP.P1".into());
    s.style = "LIGHTWEIGHT".into();
    s.args = vec![
        Arg {
            value: Some("5".into()),
            ..Arg::default()
        },
        Arg::default(),
    ];
    s.when = When::Named("APP.S1".into());
    s.enabled = false;
    let script = job::create_block(&s, &all_cols()).script();
    assert!(
        script.contains("program_name => '\"APP\".\"P1\"'")
            && script.contains("schedule_name => '\"APP\".\"S1\"'"),
        "{script}"
    );
    assert!(script.contains("job_style => 'LIGHTWEIGHT'"), "{script}");
    assert!(
        script.contains("argument_position => 1,\n      argument_value => '5'"),
        "{script}"
    );
    assert!(!script.contains("argument_position => 2"), "{script}");
    assert!(!script.contains("DBMS_SCHEDULER.ENABLE"), "{script}");
    // A chain.
    let mut s = job_spec();
    s.action = Action::Chain("APP.C1".into());
    s.when = When::Immediate;
    let script = job::create_block(&s, &all_cols()).script();
    assert!(
        script.contains("job_type => 'CHAIN',\n      job_action => '\"APP\".\"C1\"'"),
        "{script}"
    );
}

#[test]
fn a_job_edit_changes_only_what_changed_and_disables_it_around_its_action() {
    let old = job_spec();
    let mut new = old.clone();
    assert_eq!(job::alter_block(&old, &new, &all_cols(), &[]).calls(), 0);
    new.comments = Some("x".into());
    new.priority = 4;
    let b = job::alter_block(&old, &new, &all_cols(), &[]);
    let s = b.script();
    assert_eq!(b.calls(), 2, "{s}");
    assert!(!s.contains("DISABLE"), "comments don't disable it: {s}");
    one_block(&s);
    // What it runs changes: disabled first, enabled again at the end.
    let mut new = old.clone();
    new.action = Action::Inline {
        kind: "STORED_PROCEDURE".into(),
        action: "APP.PROC".into(),
    };
    new.args = vec![
        Arg {
            value: Some("1".into()),
            ..Arg::default()
        },
        Arg {
            value: Some("x'y".into()),
            ..Arg::default()
        },
    ];
    let s = job::alter_block(&old, &new, &all_cols(), &[]).script();
    let order = [
        "DBMS_SCHEDULER.DISABLE(name => '\"APP\".\"J1\"', force => TRUE);",
        "disabled := TRUE;",
        "'job_type', 'STORED_PROCEDURE'",
        "'job_action', 'APP.PROC'",
        "'number_of_arguments', 2",
        "argument_value => '1'",
        "argument_value => 'x''y'",
        "DBMS_SCHEDULER.ENABLE('\"APP\".\"J1\"');\n  disabled := FALSE;",
    ];
    let mut at = 0;
    for o in order {
        let i = s[at..]
            .find(o)
            .unwrap_or_else(|| panic!("{o} after {at}:\n{s}"))
            + at;
        at = i;
    }
    one_block(&s);
    // To a program: its own action and count go first.
    let mut prog = new.clone();
    prog.action = Action::Program("APP.P1".into());
    prog.args = vec![Arg {
        value: Some("9".into()),
        ..Arg::default()
    }];
    let s = job::alter_block(&new, &prog, &all_cols(), &[]).script();
    let reset = s.find("RESET_JOB_ARGUMENT_VALUE").unwrap();
    let nulls = s
        .find("SET_ATTRIBUTE_NULL('\"APP\".\"J1\"', 'job_action')")
        .unwrap();
    let count = s
        .find("SET_ATTRIBUTE_NULL('\"APP\".\"J1\"', 'number_of_arguments')")
        .unwrap();
    let program = s.find("'program_name', '\"APP\".\"P1\"'").unwrap();
    assert!(reset < nulls && nulls < count && count < program, "{s}");
    assert_eq!(s.matches("RESET_JOB_ARGUMENT_VALUE").count(), 2, "{s}");
    // Back to a block: the program goes, and the count is 0.
    let s = job::alter_block(&prog, &old, &all_cols(), &[]).script();
    assert!(
        s.contains("SET_ATTRIBUTE_NULL('\"APP\".\"J1\"', 'program_name')")
            && s.contains("'number_of_arguments', 0"),
        "{s}"
    );
    // Fewer arguments: the ones that go are reset first.
    let mut fewer = new.clone();
    fewer.args.truncate(1);
    let s = job::alter_block(&new, &fewer, &all_cols(), &[]).script();
    assert!(
        s.find("argument_position => 2);").unwrap() < s.find("'number_of_arguments', 1").unwrap(),
        "{s}"
    );
    // Disabled: no disable first; enabled at the end only when asked.
    let mut off = old.clone();
    off.enabled = false;
    let mut on = off.clone();
    on.enabled = true;
    let s = job::alter_block(&off, &on, &all_cols(), &[]).script();
    assert!(!s.contains("DISABLE") && s.contains("ENABLE"), "{s}");
    let s = job::alter_block(&on, &off, &all_cols(), &[]).script();
    assert!(
        s.contains("DBMS_SCHEDULER.DISABLE(name => '\"APP\".\"J1\"', force => TRUE)"),
        "{s}"
    );
    // An enabled job that changes mode: from a named schedule to an
    // expression. The calendar string is checked before anything changes;
    // a failure after the disable leaves it disabled (never enabled again
    // with its schedule nulled: it would run at once, and an auto_drop job
    // would be dropped after).
    let mut named = old.clone();
    named.when = When::Named("APP.S1".into());
    let mut expr = old.clone();
    expr.when = When::Expression {
        start: None,
        expr: "freq=weekly; byday=MON".into(),
        end: None,
    };
    let b = job::alter_block(&named, &expr, &all_cols(), &[]);
    let s = b.script();
    let check = s
        .find("DBMS_SCHEDULER.EVALUATE_CALENDAR_STRING(\n      calendar_string => 'freq=weekly; byday=MON',\n      start_date => NULL,\n      return_date_after => NULL,\n      next_run_date => next_run);")
        .unwrap_or_else(|| panic!("{s}"));
    let disable = s.find("DBMS_SCHEDULER.DISABLE").unwrap();
    let null = s
        .find("SET_ATTRIBUTE_NULL('\"APP\".\"J1\"', 'schedule_name')")
        .unwrap();
    assert!(check < disable && disable < null, "{s}");
    assert!(s.contains("  next_run TIMESTAMP WITH TIME ZONE;\n"), "{s}");
    let handler = &s[s.find("EXCEPTION").unwrap()..];
    assert!(
        !handler.contains("ENABLE") && handler.contains("RAISE_APPLICATION_ERROR(-20991"),
        "{handler}"
    );
    // The check isn't a change: DISABLE, null, set, ENABLE.
    assert_eq!(b.calls(), 4, "{s}");
    // Disabled in the same save: still disabled after a failure.
    let mut off = expr.clone();
    off.enabled = false;
    let s = job::alter_block(&named, &off, &all_cols(), &[]).script();
    assert!(!s.contains("disabled := FALSE"), "{s}");
    // A PL/SQL expression, or the same calendar string: nothing checked.
    let mut plsql = old.clone();
    plsql.when = When::Expression {
        start: None,
        expr: "SYSTIMESTAMP + 1".into(),
        end: None,
    };
    let s = job::alter_block(&named, &plsql, &all_cols(), &[]).script();
    assert!(!s.contains("EVALUATE_CALENDAR_STRING"), "{s}");
    let mut same = old.clone();
    same.comments = Some("c".into());
    let s = job::alter_block(&old, &same, &all_cols(), &[]).script();
    assert!(!s.contains("EVALUATE_CALENDAR_STRING"), "{s}");
    // Notifications are removed and added again.
    let mut n = old.clone();
    n.notifications = vec![Notification {
        recipients: vec!["a@b.c".into()],
        events: vec!["JOB_FAILED".into()],
        sender: None,
        subject: None,
        body: None,
        filter: None,
    }];
    let s = job::alter_block(&n, &old, &all_cols(), &[]).script();
    assert!(
        s.contains("REMOVE_JOB_EMAIL_NOTIFICATION") && !s.contains("ADD_JOB"),
        "{s}"
    );
}

#[test]
fn job_values_round_trip_and_bad_ones_are_refused() {
    let c = ctx();
    let cat = catalog();
    let mut s = job_spec();
    s.class = "SYS.NIGHT".into();
    s.credential = Some("APP.CRED".into());
    s.raise_events = vec!["JOB_FAILED".into()];
    s.max_runs = Some(4);
    s.args = Vec::new();
    let (values, lists) = job::values_of(&s);
    let back = job::spec_of(
        &FormValues {
            values: values.clone(),
            lists: lists.clone(),
        },
        &c,
        &all_cols(),
        &cat,
        Some(&s),
    )
    .unwrap();
    assert_eq!(back, s);
    let bad = |patch: serde_json::Value| -> String {
        let mut v = values.clone();
        for (k, x) in patch.as_object().unwrap() {
            v.insert(k.clone(), x.clone());
        }
        job::spec_of(
            &FormValues {
                values: v,
                lists: lists.clone(),
            },
            &c,
            &all_cols(),
            &cat,
            Some(&s),
        )
        .unwrap_err()
        .to_string()
    };
    assert!(bad(json!({"job_type": "NOPE"})).contains("Tipo"));
    assert!(bad(json!({"plsql": "  "})).contains("Falta la acción"));
    assert!(bad(json!({"job_priority": 9})).contains("Prioridad"));
    assert!(bad(json!({"job_class": "SYS.OTHER"})).contains("Clase"));
    assert!(bad(json!({"credential": "APP.X'"})).contains("Credencial"));
    assert!(bad(json!({"raise_events": ["JOB_EXPLODED"]})).contains("evento"));
    assert!(bad(json!({"job_type": "CHAIN", "chain": "APP.NOPE"})).contains("Cadena"));
    assert!(bad(json!({"plsql": "x".repeat(4001)})).contains("4000"));
    assert!(bad(json!({"schedule_mode": "named", "schedule": "APP.S9"})).contains("Programación"));
    // Arguments for a block that takes none: the form hides the list, and
    // what it still holds doesn't apply.
    let mut l = lists.clone();
    l.insert(
        "arguments".into(),
        vec![
            map(json!({"value": "1"})),
            map(json!({"value": "x".repeat(4001)})),
        ],
    );
    let back = job::spec_of(
        &FormValues {
            values: values.clone(),
            lists: l,
        },
        &c,
        &all_cols(),
        &cat,
        Some(&s),
    )
    .unwrap();
    assert!(back.args.is_empty(), "{:?}", back.args);
    // A lightweight job runs a program.
    let v = fv(
        json!({"name": "j2", "job_style": "LIGHTWEIGHT", "plsql": "BEGIN NULL; END;", "schedule_mode": "immediate"}),
        json!({}),
    );
    assert!(job::spec_of(&v, &c, &all_cols(), &cat, None)
        .unwrap_err()
        .to_string()
        .contains("programa"));
    // Autonomous: no external jobs.
    let cloud = Ctx {
        cloud: true,
        ..ctx()
    };
    let v = fv(
        json!({"name": "j3", "job_type": "EXECUTABLE", "executable": "/bin/true", "schedule_mode": "immediate"}),
        json!({}),
    );
    assert!(job::spec_of(&v, &cloud, &all_cols(), &cat, None)
        .unwrap_err()
        .to_string()
        .contains("Autonomous"));
    // Notifications: addresses and events checked.
    let mut l = lists.clone();
    l.insert(
        "notifications".into(),
        vec![map(
            json!({"recipients": "not-a-mail", "events": ["JOB_FAILED"]}),
        )],
    );
    assert!(job::spec_of(
        &FormValues {
            values: values.clone(),
            lists: l
        },
        &c,
        &all_cols(),
        &cat,
        Some(&s)
    )
    .is_err());
    // A new job's name folds.
    let v = fv(
        json!({"name": "nuevo", "plsql": "BEGIN NULL; END;", "schedule_mode": "immediate"}),
        json!({}),
    );
    let n = job::spec_of(&v, &c, &all_cols(), &cat, None).unwrap();
    assert_eq!((n.owner.as_str(), n.name.as_str()), ("APP", "NUEVO"));
}

#[test]
fn events_and_notifications_read_as_the_designer_shows_them() {
    assert_eq!(
        job::events_of("JOB_FAILED,JOB_STARTED"),
        ["JOB_STARTED", "JOB_FAILED"]
    );
    assert_eq!(
        job::events_of("JOB_RUN_COMPLETED"),
        ["JOB_SUCCEEDED", "JOB_FAILED", "JOB_STOPPED"]
    );
    assert_eq!(job::events_of("JOB_ALL_EVENTS").len(), job::EVENTS.len());
    let row = |r: &str, e: &str| {
        Rec::of(&[
            ("RECIPIENT", r),
            ("EVENT", e),
            ("SUBJECT", "S"),
            ("BODY", "B"),
        ])
    };
    let n = job::notifications_of(&[
        row("a@x", "JOB_FAILED"),
        row("a@x", "JOB_BROKEN"),
        row("b@x", "JOB_FAILED"),
        row("b@x", "JOB_BROKEN"),
        row("c@x", "JOB_SUCCEEDED"),
    ]);
    assert_eq!(n.len(), 2, "{n:?}");
    assert_eq!(n[0].recipients, ["a@x", "b@x"]);
    assert_eq!(n[0].events, ["JOB_FAILED", "JOB_BROKEN"]);
    assert_eq!(n[1].recipients, ["c@x"]);
}

#[test]
fn job_arguments_carry_the_programs_names() {
    let job_args = [
        Rec::of(&[("ARGUMENT_POSITION", "1"), ("VALUE", "5")]),
        Rec::of(&[("ARGUMENT_POSITION", "3"), ("ANYDATA_VALUE", "Y")]),
    ];
    let prog_args = [
        Rec::of(&[("ARGUMENT_POSITION", "1"), ("ARGUMENT_NAME", "P_N")]),
        Rec::of(&[
            ("ARGUMENT_POSITION", "2"),
            ("ARGUMENT_NAME", "P_J"),
            ("METADATA_ATTRIBUTE", "JOB_NAME"),
        ]),
    ];
    let a = job::args_of(&job_args, &prog_args, None);
    assert_eq!(a.len(), 3);
    assert_eq!(a[0].name.as_deref(), Some("P_N"));
    assert_eq!(a[0].value.as_deref(), Some("5"));
    assert_eq!(a[1].name.as_deref(), Some("P_J (JOB_NAME)"));
    assert!(a[2].anydata && a[2].value.is_none());
    assert_eq!(job::args_of(&[], &[], Some(2)).len(), 2);
}

#[test]
fn legacy_jobs_use_dbms_job_and_commit_together() {
    let s = legacy::LegacySpec {
        what: "pkg.run('x''y');".into(),
        next: Some("2030-01-01T03:00:00".into()),
        interval: Some("SYSDATE + 1".into()),
        enabled: false,
        instance: 0,
    };
    let b = legacy::create_block(&s, false);
    let script = b.script();
    one_block(&script);
    assert!(
        script.contains("DBMS_JOB.SUBMIT(\n      job => j,"),
        "{script}"
    );
    assert!(
        script.contains("next_date => TO_DATE('2030-01-01 03:00:00', 'YYYY-MM-DD HH24:MI:SS')"),
        "{script}"
    );
    assert!(
        script.contains("DBMS_JOB.BROKEN(job => j, broken => TRUE);\n  COMMIT;"),
        "{script}"
    );
    assert!(
        script.contains("DBMS_OUTPUT.PUT_LINE('Job ' || j);"),
        "{script}"
    );
    assert!(script.contains("    ROLLBACK;\n    RAISE;"), "{script}");
    let (run, _) = b.runnable();
    assert!(run.contains("  :1 := j;\n"), "{run}");
    let mut new = s.clone();
    new.enabled = true;
    new.what = "pkg.other;".into();
    let script = legacy::alter_block(7, &s, &new, s.next.as_deref(), false).script();
    assert!(
        script.contains("DBMS_JOB.WHAT(job => 7, what => 'pkg.other;');"),
        "{script}"
    );
    assert!(script.contains("DBMS_JOB.BROKEN(\n      job => 7,\n      broken => FALSE,\n      next_date => GREATEST("), "{script}");
    assert!(
        !script.contains("NEXT_DATE(") && !script.contains("INTERVAL("),
        "{script}"
    );
    // A next date the form saw, moved on by a run: not written back.
    let mut ran = s.clone();
    ran.next = Some("2030-01-02T03:00:00".into());
    let script = legacy::alter_block(7, &ran, &s, s.next.as_deref(), false).script();
    assert!(!script.contains("NEXT_DATE"), "{script}");
    assert_eq!(
        legacy::alter_block(7, &s, &s, s.next.as_deref(), false).calls(),
        0
    );
    let v = map(json!({"what": "pkg.run"}));
    assert!(legacy::spec_of(Vals(&v))
        .unwrap_err()
        .to_string()
        .contains("«;»"));
    // No interval: the text 'null', DBMS_JOB's one-shot value (its
    // INTERVAL column can't be NULL), on submit and when it's cleared.
    let once = legacy::LegacySpec {
        interval: None,
        ..s.clone()
    };
    let script = legacy::create_block(&once, false).script();
    assert!(script.contains("interval => 'null',"), "{script}");
    assert!(!script.contains("interval => NULL"), "{script}");
    let script = legacy::alter_block(7, &s, &once, s.next.as_deref(), false).script();
    assert!(
        script.contains("DBMS_JOB.INTERVAL(job => 7, interval => 'null');"),
        "{script}"
    );
    // Read back, 'null' is no interval.
    let (values, _) = (legacy::values_of(&once), ());
    assert_eq!(values["interval"], json!(""));
}

// ---------------------------------------------------------------------------
// The other kinds
// ---------------------------------------------------------------------------

#[test]
fn a_program_arguments_default_is_its_anydata_value() {
    let r = Rec::of(&[
        ("OWNER", "APP"),
        ("PROGRAM_NAME", "P1"),
        ("PROGRAM_TYPE", "STORED_PROCEDURE"),
    ]);
    let args = [
        // Without a default the view says `<NO VALUE>`.
        Rec::of(&[
            ("ARGUMENT_NAME", "P_N"),
            ("ARGUMENT_TYPE", "NUMBER"),
            ("DEFAULT_VALUE", "<NO VALUE>"),
        ]),
        Rec::of(&[
            ("ARGUMENT_NAME", "P_M"),
            ("ARGUMENT_TYPE", "NUMBER"),
            ("DEFAULT_VALUE", "5"),
            ("DEFAULT_ANYDATA_VALUE", "Y"),
        ]),
        Rec::of(&[
            ("ARGUMENT_NAME", "P_JOB"),
            ("METADATA_ATTRIBUTE", "JOB_NAME"),
            ("DEFAULT_VALUE", "<NO VALUE>"),
        ]),
    ];
    let s = program::spec_of_rec(&r, &args);
    let defaults: Vec<Option<&str>> = s.args.iter().map(|a| a.default.as_deref()).collect();
    assert_eq!(defaults, [None, Some("5"), None]);
}

fn prog() -> ProgramSpec {
    ProgramSpec {
        owner: "APP".into(),
        name: "P1".into(),
        ptype: "STORED_PROCEDURE".into(),
        action: "APP.PROC".into(),
        args: vec![
            ProgArg {
                name: Some("P_N".into()),
                metadata: "VALUE".into(),
                arg_type: Some("NUMBER".into()),
                default: Some("5".into()),
            },
            ProgArg {
                name: None,
                metadata: "JOB_NAME".into(),
                arg_type: None,
                default: None,
            },
        ],
        enabled: true,
        detached: false,
        comments: None,
    }
}

#[test]
fn programs_define_their_arguments() {
    let s = program::create_block(&prog()).script();
    one_block(&s);
    assert!(s.contains("number_of_arguments => 2"), "{s}");
    assert!(s.contains("DEFINE_PROGRAM_ARGUMENT(\n      program_name => '\"APP\".\"P1\"',\n      argument_position => 1,\n      argument_name => 'P_N',\n      argument_type => 'NUMBER',\n      default_value => '5');"), "{s}");
    assert!(s.contains("DEFINE_METADATA_ARGUMENT(\n      program_name => '\"APP\".\"P1\"',\n      metadata_attribute => 'JOB_NAME',\n      argument_position => 2,"), "{s}");
    let mut new = prog();
    new.args.truncate(1);
    new.args[0].default = Some("7".into());
    let s = program::alter_block(&prog(), &new).script();
    let drop = s.find("DROP_PROGRAM_ARGUMENT").unwrap();
    let count = s.find("'number_of_arguments', 1").unwrap();
    let define = s.find("default_value => '7'").unwrap();
    assert!(
        s.contains("disabled := TRUE;") && drop < count && count < define,
        "{s}"
    );
    let mut c = prog();
    c.comments = Some("x".into());
    let b = program::alter_block(&prog(), &c);
    assert_eq!(b.calls(), 1);
    assert!(!b.script().contains("DISABLE"));
    // Values: a plain argument needs its type; a block takes none.
    let cat = catalog();
    let v = fv(
        json!({"name": "p2", "program_type": "STORED_PROCEDURE", "procedure": "APP.X"}),
        json!({"arguments": [{"name": "a", "metadata": "VALUE"}]}),
    );
    assert!(program::spec_of(&v, &ctx(), &cat, None)
        .unwrap_err()
        .to_string()
        .contains("tipo de dato"));
    // A block takes none: the tab is hidden, what it holds doesn't apply
    // (nor is it checked: this row has no data type).
    let v = fv(
        json!({"name": "p2", "plsql": "BEGIN NULL; END;"}),
        json!({"arguments": [{"name": "a", "metadata": "VALUE"}]}),
    );
    assert!(program::spec_of(&v, &ctx(), &cat, None)
        .unwrap()
        .args
        .is_empty());
    let (values, lists) = program::values_of(&prog());
    let back =
        program::spec_of(&FormValues { values, lists }, &ctx(), &cat, Some(&prog())).unwrap();
    assert_eq!(back, prog());
}

#[test]
fn named_schedules_by_time_and_by_event() {
    let s = ScheduleSpec {
        owner: "APP".into(),
        name: "S1".into(),
        when: When::Once(ts("2030-01-01T00:00:00")),
        comments: None,
    };
    let script = schedule::create_block(&s).script();
    assert!(script.contains("CREATE_SCHEDULE(\n      schedule_name => '\"APP\".\"S1\"',\n      start_date => TO_TIMESTAMP_TZ") && script.contains("repeat_interval => NULL"), "{script}");
    let e = ScheduleSpec {
        when: When::Event {
            start: None,
            queue: "APP.Q".into(),
            condition: Some("x".into()),
            end: None,
        },
        ..s.clone()
    };
    let script = schedule::create_block(&e).script();
    assert!(
        script.contains("CREATE_EVENT_SCHEDULE") && script.contains("queue_spec => 'APP.Q'"),
        "{script}"
    );
    // An event schedule keeps its family.
    let (values, _) = (schedule::values_of(&e), ());
    let mut v = FormValues {
        values,
        lists: BTreeMap::new(),
    };
    v.values.insert("schedule_mode".into(), json!("calendar"));
    assert!(schedule::spec_of(&v, &ctx(), &catalog(), Some(&e)).is_err());
}

#[test]
fn chains_drop_rules_before_steps_and_define_again_what_changed() {
    let step = |name: &str, program: &str| Step {
        name: name.into(),
        kind: "PROGRAM".into(),
        target: program.into(),
        ..Step::default()
    };
    let rule = |name: &str, c: &str, a: &str| Rule {
        name: Some(name.into()),
        condition: c.into(),
        action: a.into(),
        comments: None,
    };
    let old = ChainSpec {
        owner: "APP".into(),
        name: "C1".into(),
        enabled: true,
        evaluation_interval: None,
        comments: None,
        steps: vec![
            step("S1", "APP.P1"),
            step("S2", "APP.P1"),
            step("S3", "APP.P1"),
        ],
        rules: vec![
            rule("R1", "TRUE", "START S1"),
            rule("R2", "S1 COMPLETED", "START S2"),
            rule("R3", "S3 COMPLETED", "END"),
        ],
    };
    let s = chain::create_block(&old).script();
    one_block(&s);
    assert!(
        s.contains("CREATE_CHAIN(chain_name => '\"APP\".\"C1\"', rule_set_name => NULL);"),
        "{s}"
    );
    assert_eq!(s.matches("DEFINE_CHAIN_STEP").count(), 3);
    assert_eq!(s.matches("DEFINE_CHAIN_RULE").count(), 3);
    let mut new = old.clone();
    new.steps.retain(|x| x.name != "S3");
    new.steps[0].flags[0] = true;
    new.steps[1].target = "APP.P2".into();
    new.steps[1].flags[1] = true;
    new.rules.retain(|r| r.name.as_deref() != Some("R3"));
    new.rules[1].action = "START S2 END".into();
    new.rules.push(Rule {
        name: None,
        condition: "S2 FAILED".into(),
        action: "END 1".into(),
        comments: None,
    });
    let s = chain::alter_block(&old, &new).script();
    let drop_rule = s.find("DROP_CHAIN_RULE").unwrap();
    let drop_step = s.find("DROP_CHAIN_STEP").unwrap();
    assert!(drop_rule < drop_step, "{s}");
    assert!(
        s.contains("step_name => 'S1', attribute => 'SKIP', value => TRUE"),
        "{s}"
    );
    assert!(s.contains("DEFINE_CHAIN_STEP(chain_name => '\"APP\".\"C1\"', step_name => 'S2', program_name => '\"APP\".\"P2\"');"), "{s}");
    assert!(s.contains("attribute => 'PAUSE'"), "{s}");
    assert_eq!(s.matches("DEFINE_CHAIN_RULE").count(), 2, "{s}");
    assert!(!s.contains("rule_name => 'R1'"), "{s}");
    assert_eq!(chain::alter_block(&old, &old).calls(), 0);
}

#[test]
fn chain_steps_and_rules_with_quoted_names_read_back_unchanged() {
    // Created quoted, in lower case: the dictionary has "paso1".
    let old = ChainSpec {
        owner: "APP".into(),
        name: "C1".into(),
        enabled: true,
        evaluation_interval: None,
        comments: None,
        steps: vec![Step {
            name: "paso1".into(),
            kind: "PROGRAM".into(),
            target: "APP.P1".into(),
            ..Step::default()
        }],
        rules: vec![Rule {
            name: Some("regla1".into()),
            condition: "TRUE".into(),
            action: "START \"paso1\"".into(),
            comments: None,
        }],
    };
    let c = ctx();
    let cat = catalog();
    let (values, lists) = chain::values_of(&old);
    let form = FormValues { values, lists };
    let back = chain::spec_of(&form, &c, &cat, Some(&old)).unwrap();
    assert_eq!(back, old);
    assert_eq!(chain::alter_block(&old, &back).calls(), 0);
    // A new step typed in lower case is still folded, as Oracle does.
    let mut form = form;
    form.lists.get_mut("steps").unwrap().push(BTreeMap::from([
        ("step_name".to_string(), json!("paso2")),
        ("step_type".to_string(), json!("PROGRAM")),
        ("program".to_string(), json!("APP.P2")),
    ]));
    let back = chain::spec_of(&form, &c, &cat, Some(&old)).unwrap();
    assert_eq!(back.steps[0].name, "paso1");
    assert_eq!(back.steps[1].name, "PASO2");
    let s = chain::alter_block(&old, &back).script();
    assert!(!s.contains("DROP_CHAIN_STEP"), "{s}");
    assert!(s.contains("step_name => 'PASO2'"), "{s}");
}

#[test]
fn only_program_steps_take_a_credential_and_a_destination() {
    let c = ctx();
    let cat = catalog();
    let old = ChainSpec {
        owner: "APP".into(),
        name: "C1".into(),
        enabled: true,
        evaluation_interval: None,
        comments: None,
        steps: vec![
            Step {
                name: "S1".into(),
                kind: "PROGRAM".into(),
                target: "APP.P1".into(),
                ..Step::default()
            },
            Step {
                name: "S2".into(),
                kind: "EVENT_SCHEDULE".into(),
                target: "APP.EVT".into(),
                credential: Some("APP.CRED".into()),
                ..Step::default()
            },
        ],
        rules: Vec::new(),
    };
    let (values, mut lists) = chain::values_of(&old);
    // The form hides the fields on the event step: what they hold there
    // doesn't apply, and the step keeps what it has.
    for r in lists.get_mut("steps").unwrap() {
        r.insert("credential".into(), json!("APP.CRED2"));
        r.insert("destination".into(), json!("APP.DB1"));
    }
    let back = chain::spec_of(&FormValues { values, lists }, &c, &cat, Some(&old)).unwrap();
    assert_eq!(back.steps[0].credential.as_deref(), Some("APP.CRED2"));
    assert_eq!(back.steps[0].destination.as_deref(), Some("APP.DB1"));
    assert_eq!(back.steps[1].credential.as_deref(), Some("APP.CRED"));
    assert_eq!(back.steps[1].destination, None);
    let s = chain::alter_block(&old, &back).script();
    assert!(!s.contains("step_name => 'S2'"), "{s}");

    // A program step with both, made a subchain: redefined, it loses them
    // (DEFINE_CHAIN_STEP clears them), and nothing sets them again on a
    // step whose form hides them.
    let old = ChainSpec {
        steps: vec![Step {
            name: "S1".into(),
            kind: "PROGRAM".into(),
            target: "APP.P1".into(),
            credential: Some("APP.CRED".into()),
            destination: Some("APP.DB1".into()),
            ..Step::default()
        }],
        ..old
    };
    let (values, mut lists) = chain::values_of(&old);
    let row = &mut lists.get_mut("steps").unwrap()[0];
    assert_eq!(row.get("credential"), Some(&json!("APP.CRED")));
    row.insert("step_type".into(), json!("SUBCHAIN"));
    row.insert("subchain".into(), json!("APP.C1"));
    let back = chain::spec_of(&FormValues { values, lists }, &c, &cat, Some(&old)).unwrap();
    assert_eq!(back.steps[0].kind, "SUBCHAIN");
    assert_eq!(back.steps[0].credential, None);
    assert_eq!(back.steps[0].destination, None);
    let s = chain::alter_block(&old, &back).script();
    assert!(s.contains("DEFINE_CHAIN_STEP"), "{s}");
    assert!(!s.contains("CREDENTIAL_NAME"), "{s}");
    assert!(!s.contains("DESTINATION_NAME"), "{s}");
}

#[test]
fn credentials_write_only_what_changed_and_never_the_password_in_clear() {
    let old = CredentialSpec {
        owner: "APP".into(),
        name: "CRED".into(),
        username: "u".into(),
        password: None,
        windows_domain: None,
        database_role: None,
        comments: None,
        enabled: true,
    };
    let mut new = old.clone();
    new.password = Some("p@ss'".into());
    let c = ctx();
    let b = credential::create_block(&new, &c);
    assert!(
        b.script().contains("DBMS_CREDENTIAL.CREATE_CREDENTIAL")
            && b.script().contains("'********'")
    );
    assert_eq!(b.runnable().1, vec!["p@ss'".to_string()]);
    new.comments = Some("c".into());
    let b = credential::alter_block(&old, &new, &c);
    assert_eq!(b.calls(), 2);
    assert!(!b.script().contains("p@ss"));
    assert_eq!(credential::alter_block(&old, &old, &c).calls(), 0);
    // 11g: the Scheduler's own calls.
    let mut v11 = ctx();
    v11.views.remove("ALL_CREDENTIALS");
    let s = credential::create_block(&new, &v11).script();
    assert!(
        s.contains("DBMS_SCHEDULER.CREATE_CREDENTIAL") && !s.contains("enabled"),
        "{s}"
    );
    let s = credential::alter_block(&old, &new, &v11).script();
    assert!(
        s.contains("DBMS_SCHEDULER.SET_ATTRIBUTE") && s.contains("'password', '********'"),
        "{s}"
    );
    // A new one needs a password.
    let v = fv(json!({"name": "c2", "username": "u"}), json!({}));
    assert!(credential::spec_of(&v, &c, &catalog(), None)
        .unwrap_err()
        .to_string()
        .contains("Contraseña"));
}

#[test]
fn windows_groups_and_classes() {
    let w = WindowSpec {
        name: "W9".into(),
        resource_plan: Some("DEFAULT_PLAN".into()),
        when: When::Once(ts("2030-01-01T00:00:00")),
        duration: 3600,
        priority: "HIGH".into(),
        enabled: false,
        comments: None,
    };
    let s = window::create_block(&w).script();
    one_block(&s);
    assert!(
        s.contains("window_name => '\"SYS\".\"W9\"'")
            && s.contains("repeat_interval => NULL")
            && s.contains("duration => NUMTODSINTERVAL(3600, 'SECOND')"),
        "{s}"
    );
    assert!(
        s.contains("DBMS_SCHEDULER.DISABLE") && s.contains("IF created THEN"),
        "{s}"
    );
    let mut w2 = w.clone();
    w2.duration = 7200;
    w2.enabled = true;
    let s = window::alter_block(&w, &w2).script();
    assert!(
        s.contains("'duration', NUMTODSINTERVAL(7200, 'SECOND')")
            && s.contains("DBMS_SCHEDULER.ENABLE"),
        "{s}"
    );

    let g = WinGroup {
        name: "G9".into(),
        members: vec!["SYS.W1".into(), "SYS.W2".into()],
        enabled: true,
        comments: None,
    };
    let c = ctx();
    let s = window::group_create(&g, &c).script();
    assert!(
        s.contains("group_type => 'WINDOW'")
            && s.contains("member => '\"SYS\".\"W1\", \"SYS\".\"W2\"'"),
        "{s}"
    );
    let mut g2 = g.clone();
    g2.members = vec!["SYS.W2".into()];
    let s = window::group_alter(&g, &g2, &c).script();
    assert!(
        s.contains("REMOVE_GROUP_MEMBER") && !s.contains("ADD_GROUP_MEMBER"),
        "{s}"
    );
    let mut v11 = ctx();
    v11.views.remove("ALL_SCHEDULER_GROUPS");
    assert!(window::group_create(&g, &v11)
        .script()
        .contains("CREATE_WINDOW_GROUP"));

    let k = ClassSpec {
        name: "K".into(),
        consumer_group: None,
        service: Some("SVC".into()),
        logging: "FULL".into(),
        log_history: Some(10),
        comments: None,
    };
    let s = class::create_block(&k).script();
    assert!(
        s.contains("logging_level => DBMS_SCHEDULER.LOGGING_FULL")
            && s.contains("log_history => 10"),
        "{s}"
    );
    let mut k2 = k.clone();
    k2.log_history = None;
    let s = class::alter_block(&k, &k2).script();
    assert!(
        s.contains("SET_ATTRIBUTE_NULL('\"SYS\".\"K\"', 'log_history')"),
        "{s}"
    );
}

#[test]
fn watchers_destinations_resources_and_incompatibilities() {
    let f = WatcherSpec {
        owner: "APP".into(),
        name: "FW".into(),
        directory: "/in".into(),
        file_name: "*.csv".into(),
        credential: "APP.CRED".into(),
        destination: Some("SYS.AGENT1".into()),
        min_size: 0,
        steady: None,
        enabled: true,
        comments: None,
    };
    let s = watcher::create_block(&f).script();
    assert!(
        s.contains("destination => '\"AGENT1\"'")
            && s.contains("enabled => FALSE")
            && s.contains("DBMS_SCHEDULER.ENABLE"),
        "{s}"
    );
    let mut f2 = f.clone();
    f2.min_size = 10;
    f2.destination = None;
    let s = watcher::alter_block(&f, &f2).script();
    assert!(
        s.contains("'min_file_size', 10")
            && s.contains("SET_ATTRIBUTE_NULL('\"APP\".\"FW\"', 'destination')"),
        "{s}"
    );

    assert_eq!(Member::parse("LOCAL", "SYS").text(), "LOCAL");
    let m = Member::parse("\"APP\".\"CRED\"@\"APP\".\"DB1\"", "SYS");
    assert_eq!(m.credential.as_deref(), Some("APP.CRED"));
    assert_eq!(m.destination, "APP.DB1");
    assert_eq!(m.text(), "\"APP\".\"CRED\"@\"APP\".\"DB1\"");
    let g = DestGroup {
        owner: "APP".into(),
        name: "DG".into(),
        group_type: "DB_DEST".into(),
        members: vec![Member::parse("LOCAL", "SYS")],
        enabled: true,
        comments: None,
    };
    let s = destination::group_create(&g).script();
    assert!(
        s.contains("group_type => 'DB_DEST'") && s.contains("member => 'LOCAL'"),
        "{s}"
    );
    let v = fv(
        json!({"name": "dg2", "group_type": "DB_DEST"}),
        json!({"members": [{"destination": "LOCAL", "credential": "APP.CRED"}]}),
    );
    assert!(destination::group_spec(&v, &ctx(), &catalog(), None)
        .unwrap_err()
        .to_string()
        .contains("LOCAL"));

    let r = ResourceSpec {
        owner: "APP".into(),
        name: "R".into(),
        units: 4,
        status: "ENFORCE_CONSTRAINTS".into(),
        level: "JOB_LEVEL".into(),
        comments: None,
        constraints: vec![("APP.J1".into(), 2)],
    };
    let s = resource::resource_create(&r).script();
    assert!(s.contains("CREATE_RESOURCE") && s.contains("SET_RESOURCE_CONSTRAINT(object_name => '\"APP\".\"J1\"', resource_name => '\"APP\".\"R\"', units => 2);"), "{s}");
    let mut r2 = r.clone();
    r2.status = "IGNORE_CONSTRAINTS".into();
    r2.constraints = vec![("APP.J2".into(), 1)];
    let s = resource::resource_alter(&r, &r2).script();
    assert!(
        s.contains("'state', 'IGNORE_CONSTRAINTS'")
            && s.contains("units => 0")
            && s.contains("'\"APP\".\"J2\"'"),
        "{s}"
    );

    let i = IncompatSpec {
        owner: "APP".into(),
        name: "I".into(),
        level: "JOB_LEVEL".into(),
        members: vec!["APP.J1".into(), "APP.J2".into()],
        enabled: true,
        comments: None,
    };
    let s = resource::incompat_create(&i).script();
    assert!(
        s.contains("object_name => '\"APP\".\"J1\", \"APP\".\"J2\"'"),
        "{s}"
    );
    let mut i2 = i.clone();
    i2.members = vec!["APP.J1".into(), "APP.P1".into()];
    let s = resource::incompat_alter(&i, &i2).script();
    assert!(
        s.find("ADD_TO_INCOMPATIBILITY").unwrap() < s.find("REMOVE_FROM_INCOMPATIBILITY").unwrap(),
        "{s}"
    );
    let v = fv(json!({"name": "i2", "members": ["APP.J1"]}), json!({}));
    assert!(resource::incompat_spec(&v, &ctx(), &catalog(), None)
        .unwrap_err()
        .to_string()
        .contains("dos"));
}

#[test]
fn scheduler_attributes_change_one_by_one() {
    let c = ctx();
    let cur = global::current(&c);
    assert_eq!(cur.get("email_server"), Some(&None));
    assert_eq!(cur.get("scheduler_disabled"), Some(&Some("FALSE".into())));
    let mut new = cur.clone();
    new.insert("email_server".into(), Some("smtp:25".into()));
    new.insert("log_history".into(), None);
    let s = global::block(&cur, &new).script();
    assert!(
        s.contains("SET_SCHEDULER_ATTRIBUTE(attribute => 'email_server', value => 'smtp:25');"),
        "{s}"
    );
    assert!(
        s.contains("SET_SCHEDULER_ATTRIBUTE(attribute => 'log_history', value => NULL);"),
        "{s}"
    );
    assert_eq!(global::block(&cur, &cur).calls(), 0);
    let v = fv(json!({"default_timezone": "Nowhere'"}), json!({}));
    assert!(global::wanted(&v, &cur, &catalog()).is_err());
}

#[test]
fn logs_read_newest_first_as_tables() {
    let t = logs::tables(
        &[Rec::of(&[
            ("LOG_DATE", "2030-01-01T00:00:00"),
            ("LOG_DATE_TZR", "UTC"),
            ("OWNER", "APP"),
            ("JOB_NAME", "J1"),
            ("OPERATION", "RUN"),
            ("STATUS", "SUCCEEDED"),
        ])],
        &[Rec::of(&[
            ("JOB_NAME", "J1"),
            ("RUN_DURATION", "3725"),
            ("ERRORS", "ORA-1"),
        ])],
        &[],
        &[],
    );
    assert_eq!(t.len(), 4);
    assert_eq!(t[0].rows[0][0], json!("2030-01-01 00:00:00 UTC"));
    assert_eq!(t[0].rows[0][1], json!("APP.J1"));
    assert_eq!(t[1].rows[0][6], json!("01:02:05"));
    assert_eq!(t[1].rows[0][9], json!("ORA-1"));
}

// ---------------------------------------------------------------------------
// DBMS_JOB against a server
// ---------------------------------------------------------------------------

/// The `DBMS_JOB` blocks run as written (from 19c they make Scheduler
/// jobs, but the calls are the same): submit, change, remove.
#[test]
#[ignore]
fn legacy_blocks_run_on_a_server() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let rest = url.strip_prefix("oracle://").unwrap();
    let (cred, addr) = rest.rsplit_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = dbine_driver::ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    let config = crate::build_config(&cfg, false).unwrap();
    let (c, _, _) = crate::open(config, None, false).unwrap();
    let s = legacy::LegacySpec {
        what: "NULL; -- dbine_test_sched_legacy".into(),
        next: Some("2030-01-01T03:00:00".into()),
        interval: Some("SYSDATE + 1".into()),
        enabled: false,
        instance: 0,
    };
    let n = script::run_returning(&c, &legacy::create_block(&s, false)).unwrap();
    // One that runs once: no interval.
    let once = legacy::LegacySpec {
        what: "NULL; -- dbine_test_sched_legacy_once".into(),
        interval: None,
        ..s.clone()
    };
    let m = script::run_returning(&c, &legacy::create_block(&once, false));
    let check = |c: &Connection, n: i64| -> (String, String) {
        let r = c
            .query_row(
                "SELECT what, broken || '|' || interval FROM user_jobs WHERE job = :1",
                &[&n],
            )
            .unwrap();
        (r.get(0).unwrap(), r.get(1).unwrap())
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eq!(check(&c, n), (s.what.clone(), "Y|SYSDATE + 1".to_string()));
        let m = *m.as_ref().expect("a DBMS_JOB job with no interval");
        // 'null' before 19c; from 19c, where DBMS_JOB makes Scheduler
        // jobs, NULL.
        let one_shot = |i: &str| matches!(i, "Y|null" | "Y|" | "N|null" | "N|");
        let (what, state) = check(&c, m);
        assert!(what == once.what && one_shot(&state), "{what} {state}");
        let mut new = s.clone();
        new.enabled = true;
        new.interval = Some("SYSDATE + 2".into());
        script::run(
            &c,
            &legacy::alter_block(n, &s, &new, s.next.as_deref(), false),
        )
        .unwrap();
        assert_eq!(check(&c, n).1, "N|SYSDATE + 2");
        // The interval cleared: it runs once.
        let mut cleared = new.clone();
        cleared.interval = None;
        script::run(
            &c,
            &legacy::alter_block(n, &new, &cleared, s.next.as_deref(), false),
        )
        .unwrap();
        let state = check(&c, n).1;
        assert!(state.starts_with('N') && one_shot(&state), "{state}");
    }));
    let remove = |n: i64| {
        script::run(&c, &{
            let mut b = Block::new("x");
            b.rollback_on_error();
            b.call("DBMS_JOB.REMOVE", vec![("job", A::I(n))]);
            b.line("COMMIT;");
            b
        })
    };
    let removed = remove(n);
    let removed_once = m.as_ref().ok().map(|m| remove(*m));
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
    removed.unwrap();
    if let Some(r) = removed_once {
        r.unwrap();
    }
}

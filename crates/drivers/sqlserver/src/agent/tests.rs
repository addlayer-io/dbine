//! Unit tests of the scheduler's scripts, plans and forms (no server).

use super::alert::{self, Alert, Raise};
use super::category::{self, Category};
use super::choices::Catalog;
use super::job::{self, JobSpec, JobState, Notify, OwnSchedule, Step, StepOp, Targets, Wanted};
use super::operator::{self, Operator};
use super::proxy::{self, Proxy};
use super::schedule::{self, Freq};
use super::*;
use crate::jobs::{has_go_line, JobRow, Raw, ScheduleRow, ServerRow};
use dbine_driver::{FormField, FormKind};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

const ID: &str = "35b8fd0b-0208-4795-a5b9-0fdc971a8414";

fn probe_sa() -> Probe {
    Probe {
        edition: 3,
        major: 16,
        sysadmin: true,
        agent_running: true,
        login: "sa".into(),
        server: "SRV".into(),
        access: true,
        ..Default::default()
    }
}

fn sa() -> Ctx {
    Ctx {
        p: probe_sa(),
        securityadmin: true,
        server_state: true,
        error_log: false,
        alerts_visible: true,
        msx: false,
        instance: None,
    }
}

fn user() -> Ctx {
    Ctx {
        p: Probe {
            sysadmin: false,
            login: "ana".into(),
            ..probe_sa()
        },
        securityadmin: false,
        server_state: false,
        error_log: false,
        alerts_visible: false,
        msx: false,
        instance: None,
    }
}

fn map(v: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

fn fv(values: Value, lists: Value) -> FormValues {
    FormValues {
        values: map(values),
        lists: serde_json::from_value(lists).unwrap(),
    }
}

/// A script the editor runs whole: one batch, no line that reads as GO,
/// a transaction with SSMS's checks.
fn one_batch(sql: &str) {
    assert!(!has_go_line(sql), "{sql}");
    assert_eq!(crate::script::batches(sql).len(), 1, "{sql}");
    assert!(sql.starts_with("-- "), "{sql}");
    assert!(sql.contains("\nBEGIN TRANSACTION;\n"), "{sql}");
    assert!(sql.contains("\nCOMMIT TRANSACTION;\n"), "{sql}");
    assert!(sql.trim_end().ends_with("EndSave:"), "{sql}");
    // Every call is checked.
    assert_eq!(
        sql.matches("EXEC @ReturnCode = ").count(),
        sql.matches("IF (@@ERROR <> 0 OR @ReturnCode <> 0) GOTO Quit")
            .count(),
        "{sql}"
    );
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
    put("login", &["sa", "ana", "DOM\\ops"]);
    put("database", &["master", "tempdb", "ventas"]);
    put("category:job", &["[Uncategorized (Local)]", "Nightly"]);
    put("category:alert", &["[Uncategorized]"]);
    put("category:operator", &["[Uncategorized]"]);
    put("operator", &["dba", "o'neil"]);
    put("alert", &["disk", "sev 17"]);
    put("schedule", &["7", "8"]);
    put("subsystem", &["TSQL", "CmdExec", "PowerShell", "Snapshot"]);
    put("proxy:CmdExec", &["", "files"]);
    put("proxy:PowerShell", &["", "files"]);
    put("target_server", &["TSX1", "TSX2"]);
    put("credential", &["svc"]);
    put("msdb_role", &["SQLAgentOperatorRole"]);
    put("server_role", &["sysadmin", "dbcreator"]);
    put("job", &[ID]);
    c
}

// ---------------------------------------------------------------------------
// Ids, values, kinds
// ---------------------------------------------------------------------------

#[test]
fn integer_ids_and_kinds_are_checked_before_the_server() {
    assert_eq!(int_id(Kind::Schedule, " 42 ").unwrap(), 42);
    assert_eq!(int_id(Kind::Category, "0").unwrap(), 0);
    for bad_id in [
        "",
        "-1",
        "1e3",
        "4.5",
        "1; DROP TABLE t",
        "' OR 1=1 --",
        "99999999999",
        "2147483648",
        "0x10",
    ] {
        for kind in [
            Kind::Schedule,
            Kind::Operator,
            Kind::Alert,
            Kind::Proxy,
            Kind::Category,
        ] {
            let e = int_id(kind, bad_id).unwrap_err();
            assert!(
                matches!(&e, Error::Query(m) if m.contains("no es un id de")),
                "{bad_id}: {e:?}"
            );
        }
    }
    // The refused text comes back short and on one line.
    let Error::Query(m) = int_id(Kind::Alert, &format!("{}\nDROP", "9".repeat(300))).unwrap_err()
    else {
        panic!()
    };
    assert!(!m.contains('\n') && m.chars().count() < 120, "{m}");
    assert_eq!(Kind::parse("job").unwrap(), Kind::Job);
    assert_eq!(Kind::parse("category").unwrap(), Kind::Category);
    assert!(matches!(Kind::parse("x'; --"), Err(Error::Query(_))));
}

#[test]
fn values_read_whatever_json_type_they_come_as() {
    let m = map(json!({
        "t": "  a b ", "n": 5, "nf": 5.0, "ns": " 7 ", "frac": 2.5, "b1": true, "b2": "true",
        "b3": 1, "b4": "no", "list": ["x", " y ", "", null, 3], "one": "z", "null": null
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
    assert!(v.int("t", "N", 0, 10).is_err());
    let Error::Query(m) = v.int("n", "Reintentos", 6, 10).unwrap_err() else {
        panic!()
    };
    assert_eq!(m, "«Reintentos» tiene que estar entre 6 y 10 (es 5).");
    assert_eq!(v.number("frac", "V").unwrap().as_deref(), Some("2.5"));
    assert_eq!(v.number("nf", "V").unwrap().as_deref(), Some("5"));
    assert!(v.bool("b1") && v.bool("b2") && v.bool("b3"));
    assert!(!v.bool("b4") && !v.bool("missing"));
    assert_eq!(v.strings("list"), ["x", "y", "3"]);
    assert_eq!(v.strings("one"), ["z"]);
}

#[test]
fn times_and_dates_as_the_form_writes_them() {
    assert_eq!(parse_time("03:00"), Some(30000));
    assert_eq!(parse_time("23:59:59"), Some(235959));
    assert_eq!(parse_time(" 7:05:09 "), Some(70509));
    for bad_time in ["24:00:00", "12:60", "12", "a:b", "1:2:3:4", "12:00:5x", ""] {
        assert_eq!(parse_time(bad_time), None, "{bad_time}");
    }
    assert_eq!(time_text(30000), "03:00:00");
    assert_eq!(parse_date("2026-11-12"), Some(20261112));
    assert_eq!(parse_date("2026-11-12T10:00:00Z"), Some(20261112));
    for bad_date in ["2026-02-29", "2026-13-01", "12/11/2026", "2026-11-12x", ""] {
        assert_eq!(parse_date(bad_date), None, "{bad_date}");
    }
    assert_eq!(date_text(20261112), "2026-11-12");
    let m = map(json!({"d": "1989-12-31", "t": "25:00"}));
    assert!(
        date_field(Vals(&m), "d", "Fecha").is_err(),
        "msdb starts at 1990"
    );
    assert!(time_field(Vals(&m), "t", "Hora", 0).is_err());
    assert_eq!(time_field(Vals(&m), "x", "Hora", 120000).unwrap(), 120000);
}

#[test]
fn the_kinds_follow_the_edition_and_the_login() {
    let keys = |i: &SchedulerInfo| i.kinds.iter().map(|k| k.key.clone()).collect::<Vec<_>>();
    let i = info_of(&sa());
    assert_eq!(
        keys(&i),
        ["job", "schedule", "operator", "alert", "proxy", "category"]
    );
    assert!(i
        .kinds
        .iter()
        .all(|k| k.can_create && k.can_edit && k.can_delete));
    assert!(i.logs && i.notes.is_empty(), "{i:?}");
    assert_eq!(i.kinds[1].label, "Programaciones");
    assert_eq!(i.kinds[1].singular, "programación");
    // A SQLAgentUserRole member: jobs and schedules, the rest to look at;
    // alerts not even that.
    let i = info_of(&user());
    assert_eq!(
        keys(&i),
        ["job", "schedule", "operator", "proxy", "category"]
    );
    for k in &i.kinds {
        let mine = matches!(k.key.as_str(), "job" | "schedule");
        assert_eq!(
            (k.can_create, k.can_edit, k.can_delete),
            (mine, mine, mine),
            "{k:?}"
        );
    }
    assert!(!i.logs);
    assert!(i.notes.contains(&ADMIN_ONLY.to_string()), "{:?}", i.notes);
    assert!(i.notes.contains(&ALERTS_HIDDEN.to_string()));
    assert!(
        i.notes.iter().any(|n| n.contains("SQLAgentUserRole")),
        "own jobs only"
    );
    // Express: everything shows, nothing changes.
    let express = Ctx {
        p: Probe {
            edition: 4,
            agent_running: false,
            ..probe_sa()
        },
        ..sa()
    };
    let i = info_of(&express);
    assert_eq!(keys(&i).len(), 6);
    assert!(i
        .kinds
        .iter()
        .all(|k| !k.can_create && !k.can_edit && !k.can_delete));
    assert!(!i.logs);
    assert!(
        i.notes.contains(&EXPRESS_READ_ONLY.to_string()),
        "{:?}",
        i.notes
    );
    // Managed Instance: no alerts or proxies.
    let mi = Ctx {
        p: Probe {
            edition: 8,
            ..probe_sa()
        },
        ..sa()
    };
    let i = info_of(&mi);
    assert_eq!(keys(&i), ["job", "schedule", "operator", "category"]);
    assert!(i.notes.contains(&MANAGED_INSTANCE.to_string()));
    // Agent stopped.
    let stopped = Ctx {
        p: Probe {
            agent_running: false,
            ..probe_sa()
        },
        ..sa()
    };
    assert!(info_of(&stopped)
        .notes
        .iter()
        .any(|n| n.contains("no está en ejecución")));
    // No Agent at all, or no access: no kinds, a note.
    for (edition, access, babelfish) in [
        (5, true, false),
        (6, true, false),
        (12, true, false),
        (3, false, false),
        (3, true, true),
    ] {
        let c = Ctx {
            p: Probe {
                edition,
                access,
                babelfish,
                ..probe_sa()
            },
            ..sa()
        };
        let i = info_of(&c);
        assert!(
            i.kinds.is_empty() && !i.logs && i.notes.len() == 1,
            "{edition} {access}: {i:?}"
        );
    }
    // The error log: securityadmin anywhere; VIEW SERVER STATE or VIEW
    // ANY ERROR LOG from 2022.
    let reader = |major: i64, sec: bool, vss: bool, log: bool| {
        Ctx {
            p: Probe {
                major,
                ..probe_sa()
            },
            securityadmin: sec,
            server_state: vss,
            error_log: log,
            ..sa()
        }
        .logs()
    };
    assert!(reader(11, true, false, false));
    assert!(reader(16, false, true, false));
    assert!(reader(16, false, false, true));
    assert!(!reader(15, false, true, false));
    assert!(!reader(15, false, false, true));
    assert!(!reader(16, false, false, false));
    // A kind the login can't have: why, by server.
    let e = user().missing_kind();
    assert!(
        matches!(&e, Error::Query(m) if m.as_str() == ALERTS_HIDDEN),
        "{e:?}"
    );
    let e = Ctx {
        p: Probe {
            edition: 8,
            ..probe_sa()
        },
        ..sa()
    }
    .missing_kind();
    assert!(
        matches!(&e, Error::Unsupported(m) if m.as_str() == MANAGED_INSTANCE),
        "{e:?}"
    );
    // The WMI namespace of the instance's own events.
    assert_eq!(
        sa().wmi_namespace(),
        r"\\.\root\Microsoft\SqlServer\ServerEvents\MSSQLSERVER"
    );
    let named = Ctx {
        instance: Some("SQL2019".into()),
        ..sa()
    };
    assert_eq!(
        named.wmi_namespace(),
        r"\\.\root\Microsoft\SqlServer\ServerEvents\SQL2019"
    );
}

// ---------------------------------------------------------------------------
// Running a script
// ---------------------------------------------------------------------------

#[test]
fn a_change_stands_or_not_by_what_the_server_said() {
    let ok = |errors: Vec<(u32, &str)>, open: bool| {
        outcome(
            errors
                .into_iter()
                .map(|(n, m)| (n, m.to_string()))
                .collect(),
            Ran::default(),
            open,
        )
    };
    assert!(ok(vec![], false).is_ok());
    // The Agent isn't running: msdb couldn't tell it, the change stands.
    assert!(ok(
        vec![(22022, "SQLServerAgent is not currently running")],
        false
    )
    .is_ok());
    // Refused: the server's own message, not the script's.
    let e = ok(
        vec![
            (14262, "The specified @name ('x') does not exist."),
            (50000, ROLLED_BACK),
        ],
        false,
    )
    .unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.contains("@name ('x')")),
        "{e:?}"
    );
    let e = ok(vec![(22022, "not running"), (50000, ROLLED_BACK)], false).unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m == ROLLED_BACK), "{e:?}");
    // A batch-ending error left the transaction open: it was rolled back.
    let e = ok(vec![(245, "Conversion failed")], true).unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m == "Conversion failed"),
        "{e:?}"
    );
    // Committed, but something answered with an error.
    let e = ok(vec![(14523, "not granted")], false).unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.starts_with("Los cambios se guardaron") && m.contains("not granted")),
        "{e:?}"
    );
    let e = ok(vec![(14523, "not granted"), (50000, AFTER_COMMIT)], false).unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.contains("not granted")),
        "{e:?}"
    );
}

#[test]
fn scripts_have_ssms_shape() {
    let mut sc = Script::new("Prueba «x»\nDROP TABLE t", &["@jobId uniqueidentifier"]);
    sc.call(
        "msdb.dbo.sp_update_job",
        vec![
            ("job_id", A::R(guid_literal(ID))),
            ("description", A::T("it's\nGO\nnext".into())),
            ("enabled", A::I(1)),
        ],
    );
    sc.call_after_commit(
        "msdb.dbo.sp_revoke_login_from_proxy",
        vec![("name", A::T("r".into())), ("proxy_id", A::I(3))],
    );
    assert_eq!(sc.calls(), 2);
    let sql = sc.finish(Some("SELECT 1 AS x;"));
    one_batch(&sql);
    assert!(sql.starts_with("-- Prueba «x»?DROP TABLE t\nSET NOCOUNT ON;\nBEGIN TRANSACTION;\nDECLARE @ReturnCode int, @jobId uniqueidentifier;\nSET @ReturnCode = 0;\n"), "{sql}");
    assert!(sql.contains("SET @text1 = CAST(N'' AS nvarchar(max)) + N'it''s' + NCHAR(10) + N'GO' + NCHAR(10) + N'next';"), "{sql}");
    assert!(sql.contains(&format!("EXEC @ReturnCode = msdb.dbo.sp_update_job @job_id = N'{ID}',\n    @description = @text1,\n    @enabled = 1;\nIF (@@ERROR <> 0 OR @ReturnCode <> 0) GOTO QuitWithRollback;")), "{sql}");
    // After the commit and the tail, with its own failure path.
    let commit = sql.find("COMMIT TRANSACTION;").unwrap();
    let revoke = sql.find("sp_revoke_login_from_proxy").unwrap();
    assert!(
        revoke > commit && sql[commit..revoke].contains("SELECT 1 AS x;"),
        "{sql}"
    );
    assert!(
        sql.contains("@proxy_id = 3;\nIF (@@ERROR <> 0 OR @ReturnCode <> 0) GOTO QuitAfterCommit;"),
        "{sql}"
    );
    assert!(sql.contains(&format!("QuitWithRollback:\n    IF (@@TRANCOUNT > 0) ROLLBACK TRANSACTION;\n    RAISERROR ({}, 16, 1);\n    GOTO EndSave;\nQuitAfterCommit:\n    RAISERROR ({}, 16, 1);\nEndSave:\n", literal(ROLLED_BACK), literal(AFTER_COMMIT))), "{sql}");
    // Without calls after the commit, no second path.
    let sql = Script::new("x", &[]).finish(None);
    assert!(!sql.contains("QuitAfterCommit"), "{sql}");
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

fn freq(t: i64, interval: i64, recurrence: i64) -> Freq {
    Freq {
        freq_type: t,
        interval,
        recurrence,
        start_date: 20261101,
        start_time: 30000,
        ..Freq::default()
    }
}

#[test]
fn every_schedule_type_reads_back_unchanged() {
    let cases = [
        freq(4, 2, 0),
        Freq {
            subday_type: 4,
            subday_interval: 15,
            start_time: 80000,
            end_time: 180000,
            end_date: 20271231,
            ..freq(4, 1, 0)
        },
        freq(8, 2 | 8 | 32, 2),
        Freq {
            subday_type: 8,
            subday_interval: 2,
            end_time: 200000,
            ..freq(8, 1, 1)
        },
        freq(16, 15, 3),
        Freq {
            relative: 16,
            ..freq(32, 9, 1)
        },
        Freq {
            relative: 2,
            ..freq(32, 2, 6)
        },
        Freq {
            subday_type: 2,
            subday_interval: 30,
            end_time: 10000,
            ..freq(4, 1, 0)
        },
        Freq {
            interval: 0,
            subday_type: 0,
            end_time: 235959,
            ..freq(1, 0, 0)
        },
        Freq {
            interval: 0,
            subday_type: 0,
            start_time: 0,
            ..freq(64, 0, 0)
        },
        Freq {
            interval: 0,
            subday_type: 0,
            start_time: 0,
            enabled: false,
            ..freq(128, 0, 0)
        },
        // Odd values other tools leave: a daily schedule with a recurrence
        // factor, subday 0.
        Freq {
            recurrence: 1,
            subday_type: 0,
            ..freq(4, 1, 0)
        },
    ];
    for f in cases {
        let v = schedule::values("s", &f);
        let (name, back) = schedule::from_values(Vals(&v), Some(&f), 20261112, false).unwrap();
        assert_eq!(name, "s");
        assert_eq!(back, f, "{v:?}");
        assert!(back.changed(&f).is_empty());
        assert!(!v["summary"].as_str().unwrap().is_empty());
    }
}

#[test]
fn new_schedules_from_the_fields() {
    let v = map(json!({
        "name": "business hours", "type": "daily", "every_days": 2, "daily_frequency": "minutes",
        "every": 15, "time": "08:00", "end_time": "18:00:00", "start_date": "2026-12-01", "end_date": "2027-06-30"
    }));
    let (_, f) = schedule::from_values(Vals(&v), None, 20261112, false).unwrap();
    assert_eq!(
        f,
        Freq {
            enabled: true,
            freq_type: 4,
            interval: 2,
            subday_type: 4,
            subday_interval: 15,
            relative: 0,
            recurrence: 0,
            start_date: 20261201,
            end_date: 20270630,
            start_time: 80000,
            end_time: 180000,
        }
    );
    let v = map(json!({
        "name": "close", "type": "monthly_relative", "relative_week": "last", "relative_day": "weekday",
        "every_months": 3, "time": "22:30:00", "enabled": false
    }));
    let (_, f) = schedule::from_values(Vals(&v), None, 20261112, false).unwrap();
    assert_eq!(
        (
            f.freq_type,
            f.interval,
            f.relative,
            f.recurrence,
            f.subday_type,
            f.start_date,
            f.end_date,
            f.enabled
        ),
        (32, 9, 16, 3, 1, 20261112, 99991231, false)
    );
    let v = map(
        json!({"name": "w", "type": "weekly", "weekdays": ["sunday", "saturday"], "every_weeks": 1}),
    );
    let (_, f) = schedule::from_values(Vals(&v), None, 20261112, false).unwrap();
    assert_eq!((f.interval, f.recurrence), (1 | 64, 1));
    let v = map(
        json!({"name": "once", "type": "once", "start_date": "2026-12-24", "time": "23:00:00"}),
    );
    let (_, f) = schedule::from_values(Vals(&v), None, 20261112, false).unwrap();
    assert_eq!(
        (
            f.freq_type,
            f.interval,
            f.subday_type,
            f.start_date,
            f.start_time
        ),
        (1, 0, 0, 20261224, 230000)
    );
}

#[test]
fn schedules_are_checked_as_msdb_would() {
    let bad = |v: Value, managed: bool, what: &str| {
        let m = map(v);
        let e = schedule::from_values(Vals(&m), None, 20261112, managed).unwrap_err();
        assert!(
            matches!(&e, Error::Query(m) if m.contains(what)),
            "{what}: {e:?}"
        );
    };
    bad(json!({"type": "daily"}), false, "necesita un nombre");
    bad(
        json!({"name": "w", "type": "weekly", "weekdays": []}),
        false,
        "al menos un día",
    );
    bad(
        json!({"name": "w", "type": "weekly", "weekdays": ["funday"]}),
        false,
        "no es un día",
    );
    bad(
        json!({"name": "s", "type": "daily", "daily_frequency": "seconds", "every": 5}),
        false,
        "entre 10",
    );
    bad(
        json!({"name": "h", "type": "daily", "daily_frequency": "hours", "every": 24}),
        false,
        "entre 1 y 23",
    );
    bad(
        json!({"name": "e", "type": "daily", "daily_frequency": "hours", "every": 1, "time": "08:00", "end_time": "08:00"}),
        false,
        "no pueden ser iguales",
    );
    bad(
        json!({"name": "d", "type": "daily", "start_date": "2027-01-01", "end_date": "2026-01-01"}),
        false,
        "termina antes",
    );
    bad(json!({"name": "t", "type": "yearly"}), false, "tipo válido");
    bad(
        json!({"name": "i", "type": "idle"}),
        true,
        "Managed Instance",
    );
    bad(
        json!({"name": "m", "type": "monthly", "day_of_month": 32}),
        false,
        "entre 1 y 31",
    );
    bad(
        json!({"name": "r", "type": "monthly_relative", "relative_week": "fifth", "relative_day": "monday"}),
        false,
        "semana del mes",
    );
    bad(
        json!({"name": "x", "type": "daily", "time": "25:00"}),
        false,
        "no es una hora",
    );
}

#[test]
fn shared_schedule_scripts() {
    let f = Freq {
        subday_type: 8,
        subday_interval: 2,
        end_time: 200000,
        ..freq(8, 2 | 32, 1)
    };
    let sql = schedule::create_script("Mon & Fri's", &f, Some("ana"), "sa");
    one_batch(&sql);
    assert!(sql.contains("EXEC @ReturnCode = msdb.dbo.sp_add_schedule @schedule_name = N'Mon & Fri''s',\n    @enabled = 1,\n    @freq_type = 8,\n    @freq_interval = 34,\n    @freq_subday_type = 8,\n    @freq_subday_interval = 2,\n    @freq_relative_interval = 0,\n    @freq_recurrence_factor = 1,\n    @active_start_date = 20261101,\n    @active_end_date = 99991231,\n    @active_start_time = 30000,\n    @active_end_time = 200000,\n    @owner_login_name = N'ana',\n    @schedule_id = @scheduleId OUTPUT;"), "{sql}");
    assert!(sql.contains("COMMIT TRANSACTION;\nSELECT @scheduleId AS schedule_id;\n"));
    // The login's own: no owner argument.
    assert!(!schedule::create_script("x", &f, Some("SA"), "sa").contains("owner_login_name"));
    // Only what changed; every freq_* when the type changes.
    assert_eq!(schedule::alter_script(5, "x", "x", &f, &f, None), None);
    let later = Freq {
        start_time: 40000,
        ..f.clone()
    };
    let sql = schedule::alter_script(5, "x", "y'z", &f, &later, None).unwrap();
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_update_schedule @schedule_id = 5,\n    @new_name = N'y''z',\n    @active_start_time = 40000;"), "{sql}");
    let monthly = freq(16, 1, 1);
    let sql = schedule::alter_script(5, "x", "x", &f, &monthly, Some("ana")).unwrap();
    for arg in [
        "@freq_type = 16",
        "@freq_interval = 1",
        "@freq_subday_type = 1",
        "@freq_subday_interval = 0",
        "@freq_relative_interval = 0",
        "@freq_recurrence_factor = 1",
        "@active_end_time = 235959",
        "@owner_login_name = N'ana'",
    ] {
        assert!(sql.contains(arg), "{arg}: {sql}");
    }
    assert!(!sql.contains("@active_start_time"), "{sql}");
    // Delete: forced (detached from its jobs) only when jobs use it.
    let sql = schedule::drop_script(5, "x", &[]);
    one_batch(&sql);
    assert!(
        sql.contains("msdb.dbo.sp_delete_schedule @schedule_id = 5;"),
        "{sql}"
    );
    let sql = schedule::drop_script(5, "x", &["a".into(), "b\nGO".into()]);
    one_batch(&sql);
    assert!(
        sql.contains("-- La usan 2 jobs: se quita de ellos (a, b?GO).\n"),
        "{sql}"
    );
    assert!(
        sql.contains("@schedule_id = 5,\n    @force_delete = 1;"),
        "{sql}"
    );
}

#[test]
fn the_schedule_fields_say_when_they_apply() {
    let fields = schedule::fields(false, 20261112);
    let keys: Vec<&str> = fields.iter().map(|f| f.key.as_str()).collect();
    for f in &fields {
        if let Some((on, values)) = &f.when {
            assert!(keys.contains(&on.as_str()), "{} depends on {on}", f.key);
            let control = fields.iter().find(|x| &x.key == on).unwrap();
            if let FormKind::Select { options } = &control.kind {
                for v in values {
                    assert!(options.iter().any(|(o, _)| o == v), "{}: {v}", f.key);
                }
            }
        }
    }
    let start = fields.iter().find(|f| f.key == "start_date").unwrap();
    assert_eq!(start.default, Some(json!("2026-11-12")));
    let types = |managed| {
        let f = schedule::fields(managed, 20261112);
        match &f.iter().find(|f| f.key == "type").unwrap().kind {
            FormKind::Select { options } => {
                options.iter().map(|(v, _)| v.clone()).collect::<Vec<_>>()
            }
            _ => panic!(),
        }
    };
    assert!(types(false).contains(&"idle".to_string()));
    assert!(!types(true).contains(&"idle".to_string()));
}

// ---------------------------------------------------------------------------
// Job steps
// ---------------------------------------------------------------------------

fn tsql(name: &str, command: &str) -> Step {
    Step {
        name: name.into(),
        subsystem: "TSQL".into(),
        command: command.into(),
        database: Some("master".into()),
        on_success: 3,
        on_fail: 2,
        ..Step::default()
    }
}

fn goto(mut s: Step, on_success: Option<i64>, on_fail: Option<i64>) -> Step {
    if let Some(p) = on_success {
        s.on_success = 4;
        s.on_success_step = p;
    }
    if let Some(p) = on_fail {
        s.on_fail = 4;
        s.on_fail_step = p;
    }
    s
}

fn w(orig: Option<i64>, step: Step) -> Wanted {
    Wanted {
        orig,
        step,
        success_target: None,
        fail_target: None,
    }
}

#[test]
fn flags_follow_the_checkboxes_and_keep_the_rest() {
    // T-SQL: append (2), history (4), log to table (8 / 16 appending).
    assert_eq!(
        job::merge_flags(0, "TSQL", true, true, true, false),
        2 | 4 | 8
    );
    assert_eq!(job::merge_flags(0, "TSQL", false, false, true, true), 16);
    // Other subsystems keep the output in history with 32.
    assert_eq!(
        job::merge_flags(0, "CmdExec", false, true, false, false),
        32
    );
    // A 4 on another type, box untouched, stays as msdb has it (see
    // `a_step_of_another_type_with_flag_4_round_trips`).
    assert_eq!(
        job::merge_flags(4, "PowerShell", false, true, false, false),
        4
    );
    // Ticked: 32 there.
    assert_eq!(
        job::merge_flags(2, "PowerShell", true, true, false, false),
        2 | 32
    );
    // Unchanged boxes keep the bits as they were (32 on T-SQL, 64).
    assert_eq!(
        job::merge_flags(32 | 64, "CmdExec", false, true, false, false),
        32 | 64
    );
    assert_eq!(job::merge_flags(32, "TSQL", false, true, false, false), 32);
    // 64 only on CmdExec; no log table for replication.
    assert_eq!(job::merge_flags(64, "TSQL", false, false, false, false), 0);
    assert_eq!(
        job::merge_flags(0, "Snapshot", false, false, true, false),
        0
    );
    // Unticking.
    assert_eq!(
        job::merge_flags(2 | 4 | 16, "TSQL", false, false, false, false),
        0
    );
}

/// A step of another type stored with flag 4 (msdb takes it only on
/// T-SQL now): opened and saved untouched, nothing changes; changed, it is
/// written with 32, the same box.
#[test]
fn a_step_of_another_type_with_flag_4_round_trips() {
    let cat = catalog();
    let cur = vec![Step {
        subsystem: "CmdExec".into(),
        database: None,
        flags: 2 | 4,
        ..tsql("copy", "copy a b")
    }];
    let names = vec!["copy".to_string()];
    let back = |edit: &dyn Fn(&mut BTreeMap<String, Value>)| {
        let mut row = job::step_row(&cur[0], 1, &cur);
        edit(&mut row);
        let w = job::wanted_step(Vals(&row), Some(1), Some(&cur[0]), &cat, true).unwrap();
        let (steps, _) = job::resolve_steps(vec![w], &names, None).unwrap();
        job::plan_steps(&cur, &steps)
    };
    assert_eq!(back(&|_| {}), vec![], "untouched: no change");
    let ops = back(&|r| {
        r.insert("command_cmdexec".into(), json!("copy a c"));
    });
    let mut sc = Script::new("t", &["@jobId uniqueidentifier"]);
    job::write_steps(&mut sc, &ops);
    let sql = sc.finish(None);
    assert!(
        sql.contains("@command = N'copy a c',\n    @flags = 34;"),
        "{sql}"
    );
    // Unticked: both bits go.
    let ops = back(&|r| {
        r.insert("output_in_history".into(), json!(false));
    });
    assert!(
        matches!(&ops[..], [StepOp::Update(1, d, _)] if d.flags == 2),
        "{ops:?}"
    );
}

/// msdb's procedures cut a longer text to their parameter without a word:
/// refused before the script.
#[test]
fn texts_longer_than_msdb_takes_are_refused() {
    assert!(fits("Nombre", &"x".repeat(128), SYSNAME).is_ok());
    let e = fits("Nombre", &"x".repeat(129), SYSNAME).unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m == "«Nombre» admite como máximo 128 caracteres (tiene 129)."),
        "{e:?}"
    );
    // Counted as nvarchar counts (UTF-16): an emoji takes two.
    assert!(fits("Nombre", &"😀".repeat(64), SYSNAME).is_ok());
    assert!(fits("Nombre", &format!("{}x", "😀".repeat(64)), SYSNAME).is_err());
    let cat = catalog();
    let long = |n: usize| "x".repeat(n);
    let refused = |r: Result<()>, what: &str| {
        let Err(e) = r else {
            panic!("{what}: accepted")
        };
        assert!(
            matches!(&e, Error::Query(m) if m.contains(what) && m.contains("admite como máximo")),
            "{what}: {e:?}"
        );
    };
    // A job: name, description, a step's name and output file.
    let job = |v: Value, l: Value| job::spec(&fv(v, l), &cat, &sa(), None).map(|_| ());
    refused(job(json!({"name": long(129)}), json!({})), "«Nombre»");
    refused(
        job(json!({"name": "j", "description": long(513)}), json!({})),
        "«Descripción»",
    );
    assert!(job(json!({"name": "j", "description": long(512)}), json!({})).is_ok());
    refused(
        job(
            json!({"name": "j"}),
            json!({"steps": [{"name": long(129)}]}),
        ),
        "«Nombre del paso»",
    );
    refused(
        job(
            json!({"name": "j"}),
            json!({"steps": [{"name": "s", "output_file": long(201)}]}),
        ),
        "«Archivo de salida»",
    );
    refused(
        job(
            json!({"name": "j"}),
            json!({"schedules": [{"name": long(129), "type": "daily"}]}),
        ),
        "«Nombre»",
    );
    // An operator's addresses.
    let op = |v: Value| operator::from_values(&fv(v, json!({})), &cat).map(|_| ());
    refused(
        op(json!({"name": "o", "email": long(101)})),
        "«Nombre de correo electrónico»",
    );
    refused(
        op(json!({"name": "o", "netsend": long(101)})),
        "«Dirección de envío de red»",
    );
    // An alert: the message, the condition, the WMI namespace and query.
    let al = |v: Value| alert::from_values(&fv(v, json!({})), &cat).map(|_| ());
    refused(
        al(
            json!({"name": "a", "type": "severity", "severity": "17", "notification_message": long(513)}),
        ),
        "«Mensaje de notificación adicional»",
    );
    refused(
        al(
            json!({"name": "a", "type": "performance", "perf_object": long(510), "perf_counter": "c", "perf_comparison": ">", "perf_value": 1}),
        ),
        "«Condición de rendimiento»",
    );
    refused(
        al(json!({"name": "a", "type": "wmi", "wmi_namespace": long(129), "wmi_query": "q"})),
        "«Espacio de nombres»",
    );
    refused(
        al(json!({"name": "a", "type": "wmi", "wmi_namespace": "n", "wmi_query": long(513)})),
        "«Consulta»",
    );
    // A proxy's description.
    refused(
        proxy::from_values(
            &fv(
                json!({"name": "p", "credential": "svc", "description": long(513)}),
                json!({}),
            ),
            &cat,
        )
        .map(|_| ()),
        "«Descripción»",
    );
}

#[test]
fn step_rows_are_checked_against_the_server() {
    let cat = catalog();
    let row = |v: Value| map(v);
    let ok =
        |v: &BTreeMap<String, Value>, admin| job::wanted_step(Vals(v), None, None, &cat, admin);
    let r = row(
        json!({"name": "a", "subsystem": "cmdexec", "command_cmdexec": "dir", "proxy_cmdexec": "FILES",
                       "success_code": 3, "database": "ventas", "on_success": "goto", "on_success_step": "b"}),
    );
    let s = ok(&r, true).unwrap();
    assert_eq!(s.step.subsystem, "CmdExec");
    assert_eq!(s.step.command, "dir");
    assert_eq!(
        s.step.proxy.as_deref(),
        Some("files"),
        "the server's spelling"
    );
    assert_eq!(s.step.database, None, "no database for CmdExec");
    assert_eq!(s.step.success_code, 3);
    assert_eq!(s.success_target.as_deref(), Some("b"));
    // T-SQL: the database (default master), no proxy whatever was typed.
    let s = ok(
        &row(json!({"name": "t", "command": " SELECT 1 ", "proxy_cmdexec": "files"})),
        true,
    )
    .unwrap();
    assert_eq!(
        (
            s.step.subsystem.as_str(),
            s.step.database.as_deref(),
            s.step.proxy.as_deref()
        ),
        ("TSQL", Some("master"), None)
    );
    assert_eq!(s.step.command, " SELECT 1 ", "commands keep their spaces");
    for (v, what) in [
        (json!({"subsystem": "TSQL"}), "necesita un nombre"),
        (
            json!({"name": "x", "subsystem": "SSIS"}),
            "no está disponible",
        ),
        (
            json!({"name": "x", "subsystem": "Bogus"}),
            "no está disponible",
        ),
        (json!({"name": "x", "database": "nope"}), "no existe"),
        (
            json!({"name": "x", "subsystem": "CmdExec", "proxy_cmdexec": "other"}),
            "proxy «other»",
        ),
        (json!({"name": "x", "on_fail": "goto"}), "elegí a cuál"),
        (json!({"name": "x", "on_success": "jump"}), "acción válida"),
        (json!({"name": "x", "retry_attempts": 10000}), "Reintentos"),
    ] {
        let e = ok(&row(v.clone()), true).unwrap_err();
        assert!(
            matches!(&e, Error::Query(m) if m.contains(what)),
            "{v}: {e:?}"
        );
    }
    // Without sysadmin, what only sysadmin sets stays as it was.
    let current = Step {
        output_file: Some("/var/out.txt".into()),
        database_user: Some("etl".into()),
        ..tsql("t", "x")
    };
    let r = row(
        json!({"name": "t", "command": "x", "output_file": "/tmp/evil", "database_user": "dbo"}),
    );
    let s = job::wanted_step(Vals(&r), Some(1), Some(&current), &cat, false).unwrap();
    assert_eq!(s.step.output_file.as_deref(), Some("/var/out.txt"));
    assert_eq!(s.step.database_user.as_deref(), Some("etl"));
    // A step of a type the Agent doesn't list here keeps it.
    let ssis = Step {
        subsystem: "SSIS".into(),
        ..tsql("p", "/ISSERVER x")
    };
    let r = row(json!({"name": "p", "subsystem": "SSIS", "command_other": "/ISSERVER y"}));
    let s = job::wanted_step(Vals(&r), Some(1), Some(&ssis), &cat, true).unwrap();
    assert_eq!(s.step.command, "/ISSERVER y");
}

#[test]
fn steps_resolve_their_references_by_name() {
    let mut a = w(Some(1), tsql("a", "1"));
    a.success_target = Some("C".into());
    let mut c = w(None, tsql("c", "3"));
    c.fail_target = Some("a".into());
    let b = w(Some(1), tsql("b", "2")); // repeats a's id: a new step
    let (steps, start) = job::resolve_steps(vec![a, b, c], &names(&["a", "b"]), Some("b")).unwrap();
    assert_eq!(steps[0].step.on_success_step, 3);
    assert_eq!(steps[2].step.on_fail_step, 1);
    assert_eq!(steps[1].orig, None);
    assert_eq!(start, 2);
    let (_, start) = job::resolve_steps(vec![w(None, tsql("a", ""))], &[], None).unwrap();
    assert_eq!(start, 1);
    assert_eq!(job::resolve_steps(vec![], &[], Some("x")).unwrap().1, 0);
    // An id the job doesn't have is a new step.
    let (steps, _) =
        job::resolve_steps(vec![w(Some(9), tsql("a", ""))], &names(&["a", "b"]), None).unwrap();
    assert_eq!(steps[0].orig, None);
    let fail = |steps: Vec<Wanted>, start: Option<&str>, what: &str| {
        let e = job::resolve_steps(steps, &[], start).unwrap_err();
        assert!(
            matches!(&e, Error::Query(m) if m.contains(what)),
            "{what}: {e:?}"
        );
    };
    fail(
        vec![w(None, tsql("a", "")), w(None, tsql("A", ""))],
        None,
        "dos pasos llamados",
    );
    let mut x = w(None, tsql("x", ""));
    x.success_target = Some("nope".into());
    fail(vec![x], None, "que no está en el job");
    let mut x = w(None, tsql("x", ""));
    x.fail_target = Some("x".into());
    fail(vec![x], None, "a sí mismo");
    fail(vec![w(None, tsql("x", ""))], Some("y"), "paso inicial");
    // References that still name a step as it was before a rename.
    let mut a = w(Some(2), tsql("a2", ""));
    a.fail_target = Some("b".into());
    let b = w(Some(1), tsql("b2", ""));
    let (steps, start) = job::resolve_steps(vec![a, b], &names(&["b", "a"]), Some("a")).unwrap();
    assert_eq!((steps[0].step.on_fail_step, start), (2, 1));
    // A name in use now wins over an old one.
    let mut a = w(Some(1), tsql("x", ""));
    a.success_target = Some("y".into());
    let y = w(Some(2), tsql("y", ""));
    let z = w(None, tsql("z", ""));
    let (steps, _) = job::resolve_steps(vec![a, z, y], &names(&["y", "q"]), None).unwrap();
    assert_eq!(steps[0].step.on_success_step, 3);
}

fn names(n: &[&str]) -> Vec<String> {
    n.iter().map(|s| s.to_string()).collect()
}

#[test]
fn unchanged_steps_plan_nothing() {
    let cur = vec![
        goto(tsql("a", "1"), Some(3), None),
        tsql("b", "2"),
        tsql("c", "3"),
    ];
    let finals: Vec<Wanted> = cur
        .iter()
        .enumerate()
        .map(|(i, s)| w(Some(i as i64 + 1), s.clone()))
        .collect();
    assert!(job::plan_steps(&cur, &finals).is_empty());
}

#[test]
fn a_rename_updates_only_the_name() {
    let cur = vec![tsql("a", "1"), tsql("b", "2")];
    let finals = vec![w(Some(1), tsql("a", "1")), w(Some(2), tsql("b2", "2"))];
    let ops = job::plan_steps(&cur, &finals);
    assert_eq!(
        ops,
        vec![StepOp::Update(
            2,
            Box::new(tsql("b2", "2")),
            Box::new(tsql("b", "2"))
        )]
    );
    let changes = job::step_changes(&tsql("b2", "2"), &tsql("b", "2"));
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].0, "step_name");
}

#[test]
fn deleting_a_step_repairs_what_msdb_breaks() {
    // msdb's sp_delete_jobstep moves references past the deleted step down
    // and then turns every reference to the deleted id into "quit": "a"
    // going to "c" (3) becomes 2 and then "quit with success".
    let cur = vec![
        goto(tsql("a", "1"), Some(3), None),
        tsql("b", "2"),
        tsql("c", "3"),
    ];
    let finals = vec![
        w(Some(1), goto(tsql("a", "1"), Some(2), None)),
        w(Some(3), tsql("c", "3")),
    ];
    let ops = job::plan_steps(&cur, &finals);
    assert_eq!(ops.len(), 2, "{ops:?}");
    assert_eq!(ops[0], StepOp::Delete(2));
    let StepOp::Update(1, want, had) = &ops[1] else {
        panic!("{ops:?}")
    };
    assert_eq!(
        (had.on_success, had.on_success_step),
        (1, 0),
        "as msdb leaves it"
    );
    assert_eq!((want.on_success, want.on_success_step), (4, 2));
    // References before the deleted step are left alone.
    let cur = vec![
        tsql("a", "1"),
        tsql("b", "2"),
        goto(tsql("c", "3"), None, Some(1)),
    ];
    let finals = vec![
        w(Some(1), tsql("a", "1")),
        w(Some(3), goto(tsql("c", "3"), None, Some(1))),
    ];
    assert_eq!(job::plan_steps(&cur, &finals), vec![StepOp::Delete(2)]);
    // Several go from the highest id down.
    let cur = vec![
        tsql("a", "1"),
        tsql("b", "2"),
        tsql("c", "3"),
        tsql("d", "4"),
    ];
    let finals = vec![w(Some(1), tsql("a", "1")), w(Some(3), tsql("c", "3"))];
    assert_eq!(
        job::plan_steps(&cur, &finals),
        vec![StepOp::Delete(4), StepOp::Delete(2)]
    );
}

#[test]
fn inserting_a_step_lets_msdb_move_the_rest() {
    // "a" goes to "b" (2); a new step first: msdb moves both and the
    // reference (to 3) itself.
    let cur = vec![goto(tsql("a", "1"), Some(2), None), tsql("b", "2")];
    let n = tsql("n", "0");
    let finals = vec![
        w(None, n.clone()),
        w(Some(1), goto(tsql("a", "1"), Some(3), None)),
        w(Some(2), tsql("b", "2")),
    ];
    assert_eq!(
        job::plan_steps(&cur, &finals),
        vec![StepOp::Add(1, Box::new(n))]
    );
    // New steps referring forward: the later insert moves the earlier
    // one's reference, which is set again afterwards.
    let cur = vec![tsql("x", "x")];
    let n1 = goto(tsql("n1", "1"), Some(3), None);
    let n2 = tsql("n2", "2");
    let finals = vec![
        w(None, n1.clone()),
        w(None, n2.clone()),
        w(Some(1), tsql("x", "x")),
    ];
    let ops = job::plan_steps(&cur, &finals);
    assert_eq!(ops[0], StepOp::Add(1, Box::new(n1.clone())));
    assert_eq!(ops[1], StepOp::Add(2, Box::new(n2)));
    let StepOp::Update(1, want, had) = &ops[2] else {
        panic!("{ops:?}")
    };
    assert_eq!((had.on_success_step, want.on_success_step), (4, 3));
    assert_eq!(ops.len(), 3);
    // A new job: every step appended in order, nothing else.
    let finals = vec![
        w(None, goto(tsql("a", "1"), None, Some(2))),
        w(None, tsql("b", "2")),
    ];
    let ops = job::plan_steps(&[], &finals);
    assert_eq!(
        ops,
        vec![
            StepOp::Add(1, Box::new(finals[0].step.clone())),
            StepOp::Add(2, Box::new(finals[1].step.clone()))
        ]
    );
}

#[test]
fn reordering_rewrites_by_place_and_frees_names() {
    let cur = vec![tsql("a", "1"), tsql("b", "2")];
    let finals = vec![w(Some(2), tsql("b", "2")), w(Some(1), tsql("a", "1"))];
    let ops = job::plan_steps(&cur, &finals);
    assert_eq!(ops[0], StepOp::Rename(2, "dbine_tmp_2".into()));
    assert!(matches!(&ops[1], StepOp::Update(1, d, _) if d.name == "b"));
    assert!(matches!(&ops[2], StepOp::Update(2, d, o) if d.name == "a" && o.name == "dbine_tmp_2"));
    assert_eq!(ops.len(), 3);
    // Swapping names in place.
    let finals = vec![w(Some(1), tsql("b", "1")), w(Some(2), tsql("a", "2"))];
    let ops = job::plan_steps(&cur, &finals);
    assert_eq!(ops[0], StepOp::Rename(2, "dbine_tmp_2".into()));
    assert!(matches!(&ops[1], StepOp::Update(1, d, _) if d.name == "b"));
    assert!(matches!(&ops[2], StepOp::Update(2, d, _) if d.name == "a"));
    // A new step taking a name another one gives up.
    let finals = vec![
        w(None, tsql("a", "new")),
        w(Some(1), tsql("a old", "1")),
        w(Some(2), tsql("b", "2")),
    ];
    let ops = job::plan_steps(&cur, &finals);
    assert_eq!(ops[0], StepOp::Rename(1, "dbine_tmp_1".into()));
    assert!(matches!(&ops[1], StepOp::Add(1, d) if d.name == "a"));
    assert!(
        matches!(&ops[2], StepOp::Update(2, d, o) if d.name == "a old" && o.name == "dbine_tmp_1")
    );
    // The temporary name never collides.
    let cur = vec![tsql("a", "1"), tsql("dbine_tmp_2", "2"), tsql("b", "3")];
    let finals = vec![
        w(Some(1), tsql("a", "1")),
        w(Some(2), tsql("b", "2")),
        w(Some(3), tsql("c", "3")),
    ];
    let ops = job::plan_steps(&cur, &finals);
    assert_eq!(ops[0], StepOp::Rename(3, "dbine_tmp_3".into()));
}

#[test]
fn step_scripts_set_only_what_changed() {
    let old = Step {
        proxy: Some("files".into()),
        subsystem: "CmdExec".into(),
        database: None,
        output_file: Some("/out".into()),
        ..tsql("s", "dir")
    };
    // To T-SQL: the subsystem, the database, the proxy cleared.
    let new = Step {
        output_file: None,
        flags: 4,
        ..tsql("s", "dir")
    };
    let mut sc = Script::new("x", &["@jobId uniqueidentifier"]);
    job::write_steps(&mut sc, &[StepOp::Update(1, Box::new(new), Box::new(old))]);
    let sql = sc.finish(None);
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_update_jobstep @job_id = @jobId,\n    @step_id = 1,\n    @subsystem = N'TSQL',\n    @database_name = N'master',\n    @proxy_name = N'',\n    @output_file_name = N'',\n    @flags = 4;"), "{sql}");
    // A whole new step, with a command that has a GO line and quotes.
    let step = Step {
        retry_attempts: 2,
        retry_interval: 5,
        database_user: Some("etl".into()),
        output_file: Some("C:\\out.txt".into()),
        ..goto(
            tsql("load 'sales'", "EXEC dbo.p N'it''s';\nGO\nSELECT 1;"),
            None,
            Some(1),
        )
    };
    let mut sc = Script::new("x", &["@jobId uniqueidentifier"]);
    job::write_steps(
        &mut sc,
        &[
            StepOp::Add(2, Box::new(step)),
            StepOp::Delete(3),
            StepOp::Rename(1, "t".into()),
        ],
    );
    let sql = sc.finish(None);
    one_batch(&sql);
    assert!(sql.contains("SET @text1 = CAST(N'' AS nvarchar(max)) + N'EXEC dbo.p N''it''''s'';' + NCHAR(10) + N'GO' + NCHAR(10) + N'SELECT 1;';"), "{sql}");
    assert!(sql.contains("msdb.dbo.sp_add_jobstep @job_id = @jobId,\n    @step_id = 2,\n    @step_name = N'load ''sales''',\n    @subsystem = N'TSQL',\n    @command = @text1,\n    @database_name = N'master',\n    @database_user_name = N'etl',\n    @on_success_action = 3,\n    @on_success_step_id = 0,\n    @on_fail_action = 4,\n    @on_fail_step_id = 1,\n    @retry_attempts = 2,\n    @retry_interval = 5,\n    @os_run_priority = 0,\n    @output_file_name = N'C:\\out.txt',\n    @flags = 0;"), "{sql}");
    assert!(sql.contains("msdb.dbo.sp_delete_jobstep @job_id = @jobId,\n    @step_id = 3;"));
    assert!(sql.contains(
        "msdb.dbo.sp_update_jobstep @job_id = @jobId,\n    @step_id = 1,\n    @step_name = N't';"
    ));
    // A hidden proxy (one the login can't name) is left alone.
    let hidden = Step {
        subsystem: "CmdExec".into(),
        proxy_hidden: true,
        ..tsql("s", "dir")
    };
    assert!(job::step_changes(&hidden, &hidden).is_empty());
    // CmdExec's database isn't compared (msdb clears it).
    let cmd = Step {
        subsystem: "CmdExec".into(),
        database: None,
        ..tsql("s", "dir")
    };
    let stored = Step {
        database: Some("master".into()),
        ..cmd.clone()
    };
    assert!(job::step_changes(&cmd, &stored).is_empty());
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

fn spec() -> JobSpec {
    let (steps, start) = job::resolve_steps(
        vec![w(None, tsql("one", "SELECT 1;")), {
            let mut x = w(None, tsql("two", "SELECT 2;"));
            x.step.on_fail = 4;
            x.fail_target = Some("one".into());
            x
        }],
        &[],
        Some("one"),
    )
    .unwrap();
    JobSpec {
        name: "dbine's nightly".into(),
        enabled: true,
        description: "load".into(),
        category: Some("Nightly".into()),
        owner: Some("ana".into()),
        steps,
        start_step: start,
        schedules: vec![OwnSchedule {
            id: None,
            name: "night".into(),
            freq: freq(4, 1, 0),
        }],
        shared: vec![8],
        alerts: Some(vec!["disk".into()]),
        email: Notify {
            level: 2,
            operator: Some("o'neil".into()),
        },
        page: Notify::default(),
        netsend: Notify {
            level: 0,
            operator: Some("ignored".into()),
        },
        eventlog: 2,
        delete: 0,
        targets: Targets::Local,
    }
}

#[test]
fn the_create_script_makes_the_whole_job() {
    let sql = job::create_script(&spec(), "sa");
    one_batch(&sql);
    assert!(
        sql.starts_with("-- Job del Agente SQL Server «dbine's nightly»\n"),
        "{sql}"
    );
    assert!(sql.contains("DECLARE @ReturnCode int, @jobId uniqueidentifier;"));
    assert!(sql.contains("msdb.dbo.sp_add_job @job_name = N'dbine''s nightly',\n    @enabled = 1,\n    @notify_level_eventlog = 2,\n    @notify_level_email = 2,\n    @notify_email_operator_name = N'o''neil',\n    @notify_level_page = 0,\n    @notify_level_netsend = 0,\n    @delete_level = 0,\n    @description = N'load',\n    @category_name = N'Nightly',\n    @owner_login_name = N'ana',\n    @job_id = @jobId OUTPUT;"), "{sql}");
    assert!(!sql.contains("ignored"), "no operator without a level");
    let order = [
        "sp_add_job ",
        "@step_name = N'one'",
        "@step_name = N'two'",
        "sp_update_job @job_id = @jobId,\n    @start_step_id = 1;",
        // The job's own schedule goes to the job's owner (SSMS's way).
        "sp_add_jobschedule @job_id = @jobId,\n    @name = N'night',\n    @enabled = 1,\n    @freq_type = 4,",
        "sp_attach_schedule @job_id = @jobId,\n    @schedule_id = 8;",
        "sp_update_alert @name = N'disk',\n    @job_id = @jobId;",
        "sp_add_jobserver @job_id = @jobId,\n    @server_name = N'(local)';",
        "COMMIT TRANSACTION;\nSELECT @jobId AS job_id;",
    ];
    let mut at = 0;
    for part in order {
        let i = sql[at..]
            .find(part)
            .unwrap_or_else(|| panic!("{part} after {at}: {sql}"));
        at += i + part.len();
    }
    assert!(
        sql.contains("@on_fail_action = 4,\n    @on_fail_step_id = 1,"),
        "{sql}"
    );
    // The login's own job: no owner; multiserver targets.
    let mut j = spec();
    j.owner = Some("SA".into());
    j.targets = Targets::Servers(vec!["TSX1".into(), "TSX'2".into()]);
    j.category = None;
    let sql = job::create_script(&j, "sa");
    assert!(!sql.contains("owner_login_name"));
    assert!(sql.contains("@category_name = N'[Uncategorized (Local)]'"));
    assert!(sql.contains("@server_name = N'TSX1';") && sql.contains("@server_name = N'TSX''2';"));
    assert!(!sql.contains("(local)"));
    assert!(
        !sql.contains("sp_add_schedule") && !sql.contains("@scheduleId"),
        "{sql}"
    );
}

fn state() -> JobState {
    let s = spec();
    JobState {
        id: ID.into(),
        raw: Raw {
            job: JobRow {
                name: s.name.clone(),
                enabled: true,
                description: Some("load".into()),
                start_step: 1,
                category: Some("Nightly".into()),
                owner: Some("ana".into()),
                notify_eventlog: 2,
                notify_email: 2,
                email_operator: Some("o'neil".into()),
                version: Some(4),
                ..Default::default()
            },
            schedules: vec![
                ScheduleRow {
                    id: Some(7),
                    name: "night".into(),
                    enabled: true,
                    freq_type: 4,
                    freq_interval: 1,
                    subday_type: 1,
                    start_date: 20261101,
                    end_date: 99991231,
                    start_time: 30000,
                    end_time: 235959,
                    job_count: 1,
                    ..Default::default()
                },
                ScheduleRow {
                    id: Some(8),
                    name: "shared".into(),
                    job_count: 3,
                    ..Default::default()
                },
            ],
            servers: vec![ServerRow {
                id: 0,
                name: "SRV".into(),
                ..Default::default()
            }],
            activity: None,
        },
        steps: s.steps.iter().map(|w| w.step.clone()).collect(),
        alerts: Some(vec!["disk".into()]),
        schedule_owners: Default::default(),
    }
}

/// The spec as the form reads the state back.
fn same() -> JobSpec {
    let mut j = spec();
    for (i, w) in j.steps.iter_mut().enumerate() {
        w.orig = Some(i as i64 + 1);
    }
    j.schedules[0].id = Some(7);
    j.netsend = Notify::default();
    j
}

#[test]
fn an_unchanged_job_changes_nothing() {
    assert_eq!(job::alter_script(&state(), &same(), &sa()), None);
}

#[test]
fn the_alter_script_changes_the_job_in_place() {
    let mut j = same();
    j.name = "renamed 'job'".into();
    j.enabled = false;
    j.description = String::new();
    j.email = Notify::default();
    j.page = Notify {
        level: 3,
        operator: Some("dba".into()),
    };
    j.delete = 1;
    j.schedules[0].freq.interval = 2;
    j.schedules.push(OwnSchedule {
        id: None,
        name: "extra".into(),
        freq: freq(64, 0, 0),
    });
    j.shared = vec![];
    j.alerts = Some(vec!["sev 17".into()]);
    let sql = job::alter_script(&state(), &j, &sa()).unwrap();
    one_batch(&sql);
    assert!(sql.contains(&format!("SET @jobId = N'{ID}';")), "{sql}");
    assert!(
        !sql.contains("sp_add_job ") && !sql.contains("sp_delete_job"),
        "in place: {sql}"
    );
    assert!(sql.contains("msdb.dbo.sp_update_job @job_id = @jobId,\n    @new_name = N'renamed ''job''',\n    @enabled = 0,\n    @description = N'',\n    @notify_level_email = 0,\n    @notify_email_operator_name = N'',\n    @notify_level_page = 3,\n    @notify_page_operator_name = N'dba',\n    @delete_level = 1;"), "{sql}");
    // A shared schedule is only detached (other jobs use it).
    assert!(sql.contains("msdb.dbo.sp_detach_schedule @job_id = @jobId,\n    @schedule_id = 8,\n    @delete_unused_schedule = 0;"), "{sql}");
    assert!(
        sql.contains("msdb.dbo.sp_update_schedule @schedule_id = 7,\n    @freq_interval = 2;"),
        "{sql}"
    );
    assert!(
        sql.contains("sp_add_jobschedule @job_id = @jobId,\n    @name = N'extra',"),
        "{sql}"
    );
    assert!(sql.contains("msdb.dbo.sp_update_alert @name = N'disk',\n    @job_id = N'00000000-0000-0000-0000-000000000000';"), "{sql}");
    assert!(
        sql.contains("msdb.dbo.sp_update_alert @name = N'sev 17',\n    @job_id = @jobId;"),
        "{sql}"
    );
    assert!(!sql.contains("sp_add_jobserver") && !sql.contains("sp_delete_jobserver"));
    // Steps first, the job (and its start step) after them.
    let mut j = same();
    j.steps.swap(0, 1);
    for w in &mut j.steps {
        if w.step.on_fail == 4 {
            w.step.on_fail_step = 2;
        }
    }
    j.start_step = 2;
    let sql = job::alter_script(&state(), &j, &sa()).unwrap();
    let steps = sql.find("sp_update_jobstep").unwrap();
    let start = sql.find("@start_step_id = 2").unwrap();
    assert!(steps < start, "{sql}");
    // Targets: a job without one gets the local server; servers move.
    let mut st = state();
    st.raw.servers.clear();
    let sql = job::alter_script(&st, &same(), &sa()).unwrap();
    assert!(
        sql.contains("sp_add_jobserver @job_id = @jobId,\n    @server_name = N'(local)';"),
        "{sql}"
    );
    let mut st = state();
    st.raw.servers = vec![ServerRow {
        id: 5,
        name: "TSX1".into(),
        ..Default::default()
    }];
    let mut j = same();
    j.targets = Targets::Servers(vec!["TSX2".into()]);
    let sql = job::alter_script(&st, &j, &sa()).unwrap();
    assert!(
        sql.contains("sp_delete_jobserver @job_id = @jobId,\n    @server_name = N'TSX1';"),
        "{sql}"
    );
    assert!(
        sql.contains("sp_add_jobserver @job_id = @jobId,\n    @server_name = N'TSX2';"),
        "{sql}"
    );
    // Alerts the login can't change are left out.
    let mut j = same();
    j.alerts = None;
    let mut st = state();
    st.alerts = Some(vec!["other".into()]);
    assert_eq!(job::alter_script(&st, &j, &sa()), None);
}

/// Schedules belong to a login: msdb lets only the owner (or sysadmin)
/// change or delete one, whoever owns the job.
#[test]
fn a_job_removes_schedules_as_msdb_lets_the_login() {
    let ana = user();
    let mut st = state();
    st.schedule_owners = HashMap::from([(7, Some("ANA".to_string())), (8, Some("sa".into()))]);
    let mut j = same();
    j.schedules.clear();
    j.shared.clear();
    // The owner of the job and of schedule 7: 7 goes; 8 is shared.
    let sql = job::alter_script(&st, &j, &ana).unwrap();
    assert!(
        sql.contains("@schedule_id = 7,\n    @delete_unused_schedule = 1;"),
        "{sql}"
    );
    assert!(
        sql.contains("@schedule_id = 8,\n    @delete_unused_schedule = 0;"),
        "{sql}"
    );
    // Schedule 7 is sa's (made for the job by sysadmin): only detached.
    st.schedule_owners.insert(7, Some("sa".into()));
    let sql = job::alter_script(&st, &j, &ana).unwrap();
    assert!(
        sql.contains("@schedule_id = 7,\n    @delete_unused_schedule = 0;"),
        "{sql}"
    );
    // sysadmin deletes it; an owner that wasn't read is left to msdb.
    let sql = job::alter_script(&st, &j, &sa()).unwrap();
    assert!(
        sql.contains("@schedule_id = 7,\n    @delete_unused_schedule = 1;"),
        "{sql}"
    );
    st.schedule_owners.clear();
    let sql = job::alter_script(&st, &j, &ana).unwrap();
    assert!(
        sql.contains("@schedule_id = 7,\n    @delete_unused_schedule = 0;"),
        "{sql}"
    );
    // Changing sa's schedule from ana's own job is refused before msdb.
    let cat = catalog();
    let mut st = state();
    st.schedule_owners = HashMap::from([(7, Some("sa".to_string()))]);
    let values = |every_days: i64| {
        let mut row = schedule::values("night", &Freq::of(&st.raw.schedules[0]));
        row.insert("schedule_id".into(), json!(7));
        row.insert("every_days".into(), json!(every_days));
        FormValues {
            values: map(json!({"name": "dbine's nightly", "owner": "ana"})),
            lists: BTreeMap::from([("schedules".to_string(), vec![row])]),
        }
    };
    let e = job::spec(&values(2), &cat, &ana, Some(&st)).unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.contains("es de otro inicio de sesión")),
        "{e:?}"
    );
    // Unchanged it passes; sysadmin may change it.
    assert!(job::spec(&values(1), &cat, &ana, Some(&st)).is_ok());
    assert!(job::spec(&values(2), &cat, &sa(), Some(&st)).is_ok());
}

/// Taking every step off a job whose start step isn't 1, with any other
/// change of the job: msdb would check the old start step against no steps.
#[test]
fn a_job_without_steps_starts_at_one() {
    let mut st = state();
    st.raw.job.start_step = 2;
    let mut j = same();
    j.steps.clear();
    j.start_step = 0;
    j.description = "no steps".into();
    let sql = job::alter_script(&st, &j, &sa()).unwrap();
    assert!(
        sql.contains("@description = N'no steps',\n    @start_step_id = 1;"),
        "{sql}"
    );
    let del = sql.rfind("sp_delete_jobstep").unwrap();
    assert!(del < sql.find("sp_update_job ").unwrap(), "{sql}");
    // Nothing else of the job changes: msdb isn't asked (it keeps the
    // number without checking it).
    j.description = "load".into();
    let sql = job::alter_script(&st, &j, &sa()).unwrap();
    assert!(!sql.contains("sp_update_job "), "{sql}");
    // Already 1: not sent.
    st.raw.job.start_step = 1;
    j.description = "no steps".into();
    let sql = job::alter_script(&st, &j, &sa()).unwrap();
    assert!(!sql.contains("start_step_id"), "{sql}");
}

#[test]
fn the_drop_script_and_a_stale_form() {
    let sql = job::drop_script(ID, "a\nGO");
    one_batch(&sql);
    assert!(
        sql.starts_with("-- Borra el job del Agente SQL Server «a?GO»\n"),
        "{sql}"
    );
    assert!(sql.contains(&format!(
        "msdb.dbo.sp_delete_job @job_id = N'{ID}',\n    @delete_unused_schedule = 1;"
    )));
    let st = state();
    assert!(job::stale(&fv(json!({"version_number": 4}), json!({})), &st).is_ok());
    assert!(job::stale(&fv(json!({}), json!({})), &st).is_ok());
    let e = job::stale(&fv(json!({"version_number": 3}), json!({})), &st).unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.contains("recargalo")),
        "{e:?}"
    );
}

#[test]
fn the_job_spec_checks_the_form() {
    let cat = catalog();
    let c = sa();
    let base = || {
        json!({
            "name": "j", "owner": "ANA", "category": "nightly", "notify_email": true, "email_operator": "DBA",
            "email_when": "completion", "notify_eventlog": true, "eventlog_when": "success", "delete_job": false,
            "start_step": "s1", "target": "local"
        })
    };
    let lists = json!({
        "steps": [{"name": "s1", "command": "SELECT 1"}],
        "schedules": [{"name": "n", "type": "daily"}],
        "shared_schedules": [{"schedule": "8"}, {"schedule": "8"}, {"schedule": ""}],
        "alerts": [{"alert": "DISK"}, {"alert": "disk"}]
    });
    let j = job::spec(&fv(base(), lists.clone()), &cat, &c, None).unwrap();
    assert_eq!(j.owner.as_deref(), Some("ana"));
    assert_eq!(j.category.as_deref(), Some("Nightly"));
    assert_eq!(
        j.email,
        Notify {
            level: 3,
            operator: Some("dba".into())
        }
    );
    assert_eq!((j.eventlog, j.delete, j.start_step), (1, 0, 1));
    assert_eq!(j.shared, vec![8]);
    assert_eq!(j.alerts, Some(vec!["disk".to_string()]));
    assert_eq!(j.targets, Targets::Local);
    // Not sysadmin: no owner, alerts left as they are.
    let j = job::spec(&fv(base(), lists.clone()), &cat, &user(), None).unwrap();
    assert_eq!((j.owner, j.alerts), (None, None));
    let fail = |patch: Value, lists: Value, c: &Ctx, what: &str| {
        let mut v = base();
        for (k, x) in patch.as_object().unwrap() {
            v[k] = x.clone();
        }
        let e = job::spec(&fv(v, lists), &cat, c, None).unwrap_err();
        assert!(
            matches!(&e, Error::Query(m) if m.contains(what)),
            "{what}: {e:?}"
        );
    };
    fail(json!({"name": ""}), json!({}), &c, "necesita un nombre");
    fail(
        json!({"email_operator": ""}),
        json!({}),
        &c,
        "elegí el operador",
    );
    fail(
        json!({"email_operator": "nobody"}),
        json!({}),
        &c,
        "no existe",
    );
    fail(json!({"category": "x"}), json!({}), &c, "categoría");
    fail(json!({"owner": "x"}), json!({}), &c, "inicio de sesión");
    fail(
        json!({}),
        json!({"shared_schedules": [{"schedule": "99"}]}),
        &c,
        "no existe",
    );
    fail(
        json!({}),
        json!({"shared_schedules": [{"schedule": "8; DROP"}]}),
        &c,
        "no existe",
    );
    fail(
        json!({}),
        json!({"alerts": [{"alert": "x"}]}),
        &c,
        "La alerta",
    );
    fail(
        json!({"start_step": "nope"}),
        json!({"steps": [{"name": "s1"}]}),
        &c,
        "paso inicial",
    );
    let msx = Ctx { msx: true, ..sa() };
    fail(
        json!({"target": "multi", "target_servers": []}),
        json!({}),
        &msx,
        "al menos un servidor",
    );
    fail(
        json!({"target": "multi", "target_servers": ["nope"]}),
        json!({}),
        &msx,
        "no es un servidor de destino",
    );
    let mut v = base();
    v["target"] = json!("multi");
    v["target_servers"] = json!(["tsx2"]);
    let j = job::spec(&fv(v, json!({})), &cat, &msx, None).unwrap();
    assert_eq!(j.targets, Targets::Servers(vec!["TSX2".into()]));
}

#[test]
fn the_step_editor_matches_what_the_planner_reads() {
    let cat = catalog();
    let fields = job::step_fields(&cat, true, &[]);
    let keys: Vec<&str> = fields.iter().map(|f| f.key.as_str()).collect();
    for f in &fields {
        if let Some((on, values)) = &f.when {
            let control = fields
                .iter()
                .find(|x| &x.key == on)
                .unwrap_or_else(|| panic!("{on}"));
            match &control.kind {
                FormKind::Select { options } => {
                    for v in values {
                        // A subsystem the Agent doesn't list here may be
                        // filtered out; the rest must exist.
                        if on != "subsystem" {
                            assert!(options.iter().any(|(o, _)| o == v), "{}: {v}", f.key);
                        }
                    }
                }
                FormKind::Bool => assert_eq!(values, &["true"]),
                other => panic!("{on}: {other:?}"),
            }
        }
        if let FormKind::Ref { source, .. } = &f.kind {
            assert!(
                source == "list:steps" || source == "database" || source.starts_with("proxy:"),
                "{source}"
            );
        }
    }
    for k in [
        "name",
        "subsystem",
        "command",
        "command_powershell",
        "command_cmdexec",
        "command_other",
        "database",
        "proxy_cmdexec",
        "on_success",
        "on_success_step",
        "on_fail",
        "on_fail_step",
        "retry_attempts",
        "retry_interval",
        "output_file",
        "append_output",
        "log_to_table",
        "append_to_table",
        "output_in_history",
        "database_user",
        "success_code",
        "server",
        "as_database",
    ] {
        assert!(keys.contains(&k), "{k}");
    }
    // Only the subsystems the Agent lists (and those the steps use).
    let subs = |current: &[Step]| match &job::step_fields(&cat, true, current)[1].kind {
        FormKind::Select { options } => options.iter().map(|(v, _)| v.clone()).collect::<Vec<_>>(),
        _ => panic!(),
    };
    assert_eq!(subs(&[]), ["TSQL", "CmdExec", "PowerShell", "Snapshot"]);
    let ssis = Step {
        subsystem: "SSIS".into(),
        ..tsql("p", "")
    };
    assert!(subs(&[ssis]).contains(&"SSIS".to_string()));
    // Without sysadmin, its fields are read-only.
    let fields = job::step_fields(&cat, false, &[]);
    for k in ["output_file", "database_user"] {
        assert!(fields.iter().find(|f| f.key == k).unwrap().read_only, "{k}");
    }
    // A step read back into a row and out again is the same step.
    let cur = vec![
        goto(tsql("a", "SELECT 1"), None, Some(2)),
        Step {
            subsystem: "CmdExec".into(),
            database: None,
            proxy: Some("files".into()),
            success_code: 5,
            flags: 2 | 32 | 64,
            retry_attempts: 3,
            retry_interval: 1,
            output_file: Some("/o".into()),
            ..tsql("b", "dir /s")
        },
    ];
    for (i, s) in cur.iter().enumerate() {
        let row = job::step_row(s, i as i64 + 1, &cur);
        assert_eq!(row["step_id"], json!(i + 1));
        let back = job::wanted_step(Vals(&row), Some(i as i64 + 1), Some(s), &cat, true).unwrap();
        let (steps, _) = job::resolve_steps(
            cur.iter()
                .enumerate()
                .map(|(j, x)| {
                    let r = job::step_row(x, j as i64 + 1, &cur);
                    job::wanted_step(Vals(&r), Some(j as i64 + 1), Some(x), &cat, true).unwrap()
                })
                .collect(),
            &cur.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            None,
        )
        .unwrap();
        assert_eq!(steps[i].step, *s, "{row:?}");
        assert_eq!(back.step.name, s.name);
    }
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

fn op() -> Operator {
    Operator {
        name: "o'neil".into(),
        enabled: true,
        email: "dba@x.com".into(),
        pager: String::new(),
        netsend: String::new(),
        pager_days: 2 | 32,
        hours: [80000, 180000, 90000, 130000, 0, 0],
        category: "[Uncategorized]".into(),
        alerts: BTreeMap::from([("disk".to_string(), 1)]),
    }
}

#[test]
fn operator_scripts() {
    let sql = operator::create_script(&op());
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_add_operator @name = N'o''neil',\n    @enabled = 1,\n    @email_address = N'dba@x.com',\n    @weekday_pager_start_time = 80000,\n    @weekday_pager_end_time = 180000,\n    @saturday_pager_start_time = 90000,\n    @saturday_pager_end_time = 130000,\n    @sunday_pager_start_time = 0,\n    @sunday_pager_end_time = 0,\n    @pager_days = 34,\n    @category_name = N'[Uncategorized]';"), "{sql}");
    assert!(sql.contains("msdb.dbo.sp_add_notification @alert_name = N'disk',\n    @operator_name = N'o''neil',\n    @notification_method = 1;"), "{sql}");
    assert_eq!(operator::alter_script(&op(), &op()), None);
    let mut new = op();
    new.name = "oneil".into();
    new.email = String::new();
    new.pager_days = 0;
    new.alerts = BTreeMap::from([("disk".to_string(), 3), ("sev 17".to_string(), 4)]);
    let mut cur = op();
    cur.alerts.insert("old".into(), 1);
    let sql = operator::alter_script(&cur, &new).unwrap();
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_update_operator @name = N'o''neil',\n    @new_name = N'oneil',\n    @email_address = N'',\n    @pager_days = 0;"), "{sql}");
    // Notifications under the new name, after the rename.
    let rename = sql.find("sp_update_operator").unwrap();
    for part in [
        "sp_delete_notification @alert_name = N'old',\n    @operator_name = N'oneil';",
        "sp_update_notification @alert_name = N'disk',\n    @operator_name = N'oneil',\n    @notification_method = 3;",
        "sp_add_notification @alert_name = N'sev 17',\n    @operator_name = N'oneil',\n    @notification_method = 4;",
    ] {
        let at = sql.find(part).unwrap_or_else(|| panic!("{part}: {sql}"));
        assert!(at > rename);
    }
    let sql = operator::drop_script("o'neil");
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_delete_operator @name = N'o''neil';"));
}

#[test]
fn operators_from_the_form() {
    let cat = catalog();
    let values = operator::values_of(&op());
    let lists = json!({"alerts": [{"alert": "DISK", "email": true}, {"alert": "sev 17"}]});
    let back = operator::from_values(
        &FormValues {
            values,
            lists: serde_json::from_value(lists).unwrap(),
        },
        &cat,
    )
    .unwrap();
    assert_eq!(back, op(), "a row without a method notifies nothing");
    for (v, what) in [
        (json!({"name": ""}), "necesita un nombre"),
        (
            json!({"name": "x", "pager_days": ["moonday"]}),
            "día de la semana",
        ),
        (json!({"name": "x", "weekday_end": "18:61"}), "hora válida"),
        (json!({"name": "x", "category": "none"}), "categoría"),
    ] {
        let e = operator::from_values(&fv(v.clone(), json!({})), &cat).unwrap_err();
        assert!(
            matches!(&e, Error::Query(m) if m.contains(what)),
            "{v}: {e:?}"
        );
    }
    let e = operator::from_values(
        &fv(
            json!({"name": "x"}),
            json!({"alerts": [{"alert": "nope", "email": true}]}),
        ),
        &cat,
    )
    .unwrap_err();
    assert!(
        matches!(&e, Error::Query(m) if m.contains("La alerta")),
        "{e:?}"
    );
    let keys: Vec<String> = operator::general_fields()
        .iter()
        .map(|f| f.key.clone())
        .collect();
    for k in [
        "name",
        "enabled",
        "email",
        "netsend",
        "pager",
        "pager_days",
        "weekday_start",
        "weekday_end",
        "saturday_start",
        "saturday_end",
        "sunday_start",
        "sunday_end",
        "category",
    ] {
        assert!(keys.contains(&k.to_string()), "{k}");
    }
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

fn alert_of(raise: Raise) -> Alert {
    Alert {
        name: "disk 'full'".into(),
        enabled: true,
        raise,
        database: String::new(),
        keyword: String::new(),
        job: None,
        include: 0,
        message: String::new(),
        delay: 0,
        category: "[Uncategorized]".into(),
        operators: BTreeMap::new(),
        reset_count: false,
    }
}

#[test]
fn alert_scripts_for_each_type() {
    let mut a = alert_of(Raise::Error(50001));
    a.database = "ventas".into();
    a.keyword = "it's".into();
    a.job = Some(ID.into());
    a.include = 1 | 4;
    a.delay = 90;
    a.message = "call ops".into();
    a.operators.insert("dba".into(), 3);
    let sql = alert::create_script(&a);
    one_batch(&sql);
    assert!(sql.contains(&format!("msdb.dbo.sp_add_alert @name = N'disk ''full''',\n    @message_id = 50001,\n    @severity = 0,\n    @enabled = 1,\n    @delay_between_responses = 90,\n    @include_event_description_in = 5,\n    @notification_message = N'call ops',\n    @database_name = N'ventas',\n    @event_description_keyword = N'it''s',\n    @job_id = N'{ID}',\n    @category_name = N'[Uncategorized]';")), "{sql}");
    assert!(sql.contains("msdb.dbo.sp_add_notification @alert_name = N'disk ''full''',\n    @operator_name = N'dba',\n    @notification_method = 3;"));
    let sql = alert::create_script(&alert_of(Raise::Severity(17)));
    assert!(
        sql.contains("@message_id = 0,\n    @severity = 17,"),
        "{sql}"
    );
    let sql = alert::create_script(&alert_of(Raise::Performance(
        "SQLServer:General Statistics|User Connections||>|100".into(),
    )));
    assert!(
        sql.contains(
            "@performance_condition = N'SQLServer:General Statistics|User Connections||>|100',"
        ),
        "{sql}"
    );
    let sql = alert::create_script(&alert_of(Raise::Wmi {
        namespace: r"\\.\root\Microsoft\SqlServer\ServerEvents\MSSQLSERVER".into(),
        query: "SELECT * FROM DEADLOCK_GRAPH".into(),
    }));
    assert!(
        sql.contains(r"@wmi_namespace = N'\\.\root\Microsoft\SqlServer\ServerEvents\MSSQLSERVER',"),
        "{sql}"
    );
    assert!(
        sql.contains("@wmi_query = N'SELECT * FROM DEADLOCK_GRAPH',"),
        "{sql}"
    );
    assert!(!sql.contains("@database_name"));
}

#[test]
fn alert_alter_scripts() {
    let mut cur = alert_of(Raise::Severity(17));
    cur.database = "ventas".into();
    cur.job = Some(ID.into());
    cur.operators.insert("dba".into(), 1);
    assert_eq!(alert::alter_script(&cur, &cur.clone()), None);
    // To a performance condition: msdb's columns for the event cleared.
    let mut a = cur.clone();
    a.raise = Raise::Performance("o|c||<|5".into());
    a.database = String::new();
    a.job = None;
    a.reset_count = true;
    a.operators.clear();
    let sql = alert::alter_script(&cur, &a).unwrap();
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_update_alert @name = N'disk ''full''',\n    @message_id = 0,\n    @severity = 0,\n    @performance_condition = N'o|c||<|5',\n    @database_name = N'',\n    @job_id = N'00000000-0000-0000-0000-000000000000',\n    @occurrence_count = 0,\n    @count_reset_date = @resetDate,\n    @count_reset_time = @resetTime;"), "{sql}");
    // The count starts again now: its date and time from the server's
    // clock, before the call.
    assert!(
        sql.contains("DECLARE @ReturnCode int, @resetAt datetime, @resetDate int, @resetTime int;"),
        "{sql}"
    );
    let set = sql.find("SET @resetAt = GETDATE();\nSET @resetDate = CONVERT(int, CONVERT(char(8), @resetAt, 112));\nSET @resetTime = DATEPART(HOUR, @resetAt) * 10000 + DATEPART(MINUTE, @resetAt) * 100 + DATEPART(SECOND, @resetAt);").expect("the reset time");
    assert!(set < sql.find("sp_update_alert").unwrap(), "{sql}");
    assert!(
        sql.contains(
            "sp_delete_notification @alert_name = N'disk ''full''',\n    @operator_name = N'dba';"
        ),
        "{sql}"
    );
    // Back to an error number.
    let b = Alert {
        raise: Raise::Error(9002),
        reset_count: false,
        ..a.clone()
    };
    let sql = alert::alter_script(&a, &b).unwrap();
    assert!(
        sql.contains("@message_id = 9002,\n    @severity = 0,\n    @performance_condition = N'';"),
        "{sql}"
    );
    // A rename keeps the notifications under the new name.
    let mut c = cur.clone();
    c.name = "disk".into();
    c.operators.insert("dba".into(), 2);
    let sql = alert::alter_script(&cur, &c).unwrap();
    assert!(sql.contains("@new_name = N'disk';"), "{sql}");
    assert!(sql.contains("sp_update_notification @alert_name = N'disk',\n    @operator_name = N'dba',\n    @notification_method = 2;"), "{sql}");
    let sql = alert::drop_script("disk 'full'");
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_delete_alert @name = N'disk ''full''';"));
}

#[test]
fn alerts_from_the_form() {
    let cat = catalog();
    for raise in [
        Raise::Error(50001),
        Raise::Severity(21),
        Raise::Performance("SQLServer:Buffer Manager|Page life expectancy||<|300".into()),
        Raise::Wmi {
            namespace: "ns".into(),
            query: "q".into(),
        },
    ] {
        let mut a = alert_of(raise);
        a.job = Some(ID.into());
        a.delay = 125;
        a.include = 2;
        let back = alert::from_values(
            &FormValues {
                values: alert::values_of(&a, "ns"),
                lists: BTreeMap::new(),
            },
            &cat,
        )
        .unwrap();
        assert_eq!(back, a);
    }
    let fail = |v: Value, what: &str| {
        let e = alert::from_values(&fv(v.clone(), json!({})), &cat).unwrap_err();
        assert!(
            matches!(&e, Error::Query(m) if m.contains(what)),
            "{v}: {e:?}"
        );
    };
    fail(
        json!({"type": "severity", "severity": "17"}),
        "necesita un nombre",
    );
    fail(json!({"name": "a", "type": "error"}), "número de error");
    fail(
        json!({"name": "a", "type": "severity", "severity": "26"}),
        "entre 1 y 25",
    );
    fail(
        json!({"name": "a", "type": "performance", "perf_object": "o", "perf_value": 1}),
        "contador",
    );
    fail(
        json!({"name": "a", "type": "performance", "perf_object": "o|x", "perf_counter": "c", "perf_comparison": ">", "perf_value": 1}),
        "«|»",
    );
    fail(
        json!({"name": "a", "type": "performance", "perf_object": "o", "perf_counter": "c", "perf_comparison": "!=", "perf_value": 1}),
        "comparación",
    );
    fail(
        json!({"name": "a", "type": "wmi", "wmi_namespace": "n"}),
        "consulta",
    );
    fail(json!({"name": "a", "type": "x"}), "tipo de alerta");
    fail(
        json!({"name": "a", "type": "severity", "severity": "17", "message_text": "x".repeat(101)}),
        "100 caracteres",
    );
    fail(
        json!({"name": "a", "type": "severity", "severity": "17", "run_job": true}),
        "Elegí el job",
    );
    fail(
        json!({"name": "a", "type": "severity", "severity": "17", "run_job": true, "job": "x' OR 1=1"}),
        "no es un id de job",
    );
    fail(
        json!({"name": "a", "type": "severity", "severity": "17", "run_job": true, "job": "00000000-0000-0000-0000-000000000001"}),
        "no existe",
    );
    fail(
        json!({"name": "a", "type": "severity", "severity": "17", "database": "nope"}),
        "base de datos",
    );
    let keys: Vec<String> = alert::general_fields(true, "ns")
        .iter()
        .map(|f| f.key.clone())
        .collect();
    for k in [
        "name",
        "enabled",
        "type",
        "message_id",
        "severity",
        "database",
        "message_text",
        "perf_object",
        "perf_counter",
        "perf_instance",
        "perf_comparison",
        "perf_value",
        "wmi_namespace",
        "wmi_query",
        "category",
    ] {
        assert!(keys.contains(&k.to_string()), "{k}");
    }
    assert!(matches!(
        alert::general_fields(false, "ns")
            .iter()
            .find(|f| f.key == "perf_object")
            .unwrap()
            .kind,
        FormKind::Text
    ));
}

// ---------------------------------------------------------------------------
// Proxies
// ---------------------------------------------------------------------------

fn prox() -> Proxy {
    Proxy {
        name: "files".into(),
        credential: "svc".into(),
        description: "copies".into(),
        enabled: true,
        subsystems: vec!["CmdExec".into(), "PowerShell".into()],
        logins: vec!["ana".into()],
        msdb_roles: vec!["SQLAgentOperatorRole".into()],
        server_roles: vec![],
    }
}

#[test]
fn proxy_scripts() {
    let mut p = prox();
    p.logins.push("DOM\\ops".into());
    let sql = proxy::create_script(&p);
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_add_proxy @proxy_name = N'files',\n    @credential_name = N'svc',\n    @enabled = 1,\n    @description = N'copies',\n    @proxy_id = @proxyId OUTPUT;"), "{sql}");
    assert!(sql.contains(
        "sp_grant_proxy_to_subsystem @proxy_id = @proxyId,\n    @subsystem_name = N'CmdExec';"
    ));
    assert!(
        sql.contains("sp_grant_login_to_proxy @login_name = N'ana',\n    @proxy_id = @proxyId;")
    );
    assert!(sql.contains(
        "sp_grant_login_to_proxy @msdb_role = N'SQLAgentOperatorRole',\n    @proxy_id = @proxyId;"
    ));
    // A Windows login is granted after the commit (msdb impersonates it).
    let commit = sql.find("COMMIT TRANSACTION;").unwrap();
    assert!(
        sql.find("@login_name = N'DOM\\ops'").unwrap() > commit,
        "{sql}"
    );
    assert!(sql.find("@login_name = N'ana'").unwrap() < commit);
    assert!(sql.contains("COMMIT TRANSACTION;\nSELECT @proxyId AS proxy_id;\n"));
    assert_eq!(proxy::alter_script(4, &prox(), &prox(), true), None);
    let mut q = prox();
    q.name = "files2".into();
    q.enabled = false;
    q.description = String::new();
    q.subsystems = vec!["PowerShell".into(), "Snapshot".into()];
    q.logins = vec![];
    q.msdb_roles = vec![];
    q.server_roles = vec!["dbcreator".into()];
    let sql = proxy::alter_script(4, &prox(), &q, true).unwrap();
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_update_proxy @proxy_id = 4,\n    @new_name = N'files2',\n    @enabled = 0;"), "{sql}");
    assert!(
        sql.contains("-- sp_update_proxy no deja vacía la descripción"),
        "{sql}"
    );
    assert!(sql.contains(
        "sp_revoke_proxy_from_subsystem @proxy_id = 4,\n    @subsystem_name = N'CmdExec';"
    ));
    assert!(sql.contains(
        "sp_grant_proxy_to_subsystem @proxy_id = 4,\n    @subsystem_name = N'Snapshot';"
    ));
    assert!(sql.contains(
        "sp_grant_login_to_proxy @fixed_server_role = N'dbcreator',\n    @proxy_id = 4;"
    ));
    // The login's revoke stays in the transaction; the role's goes after.
    let commit = sql.find("COMMIT TRANSACTION;").unwrap();
    assert!(sql.find("@name = N'ana'").unwrap() < commit, "{sql}");
    assert!(
        sql.find("@name = N'SQLAgentOperatorRole'").unwrap() > commit,
        "{sql}"
    );
    // Grants it couldn't read are left alone.
    let mut r = prox();
    r.enabled = false;
    let sql = proxy::alter_script(4, &Proxy::default(), &r, false).unwrap();
    assert!(!sql.contains("grant") && !sql.contains("revoke"), "{sql}");
    let sql = proxy::drop_script(4, "files");
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_delete_proxy @proxy_id = 4;"));
}

#[test]
fn proxies_from_the_form() {
    let cat = catalog();
    let v = fv(
        json!({"name": "files", "credential": "SVC", "description": "copies", "enabled": true, "subsystems": ["cmdexec", "PowerShell", "CmdExec"]}),
        json!({"logins": [{"login": "ANA"}, {"login": "ana"}], "msdb_roles": [{"role": "sqlagentoperatorrole"}], "server_roles": []}),
    );
    assert_eq!(proxy::from_values(&v, &cat).unwrap(), prox());
    for (v, l, what) in [
        (
            json!({"credential": "svc"}),
            json!({}),
            "necesita un nombre",
        ),
        (json!({"name": "p"}), json!({}), "credencial"),
        (
            json!({"name": "p", "credential": "x"}),
            json!({}),
            "no existe",
        ),
        (
            json!({"name": "p", "credential": "svc", "subsystems": ["TSQL"]}),
            json!({}),
            "no está disponible",
        ),
        (
            json!({"name": "p", "credential": "svc", "subsystems": ["SSIS"]}),
            json!({}),
            "no está disponible",
        ),
        (
            json!({"name": "p", "credential": "svc"}),
            json!({"logins": [{"login": "x"}]}),
            "inicio de sesión",
        ),
        (
            json!({"name": "p", "credential": "svc"}),
            json!({"msdb_roles": [{"role": "x"}]}),
            "rol de msdb",
        ),
        (
            json!({"name": "p", "credential": "svc"}),
            json!({"server_roles": [{"role": "x"}]}),
            "rol de servidor",
        ),
    ] {
        let e = proxy::from_values(&fv(v.clone(), l), &cat).unwrap_err();
        assert!(
            matches!(&e, Error::Query(m) if m.contains(what)),
            "{v}: {e:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Categories
// ---------------------------------------------------------------------------

#[test]
fn category_scripts() {
    let sql = category::create_script(1, 2, "Nightly's");
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_add_category @class = N'JOB',\n    @type = N'MULTI-SERVER',\n    @name = N'Nightly''s';"), "{sql}");
    assert!(category::create_script(2, 3, "a").contains("@class = N'ALERT',\n    @type = N'NONE',"));
    assert!(
        category::create_script(3, 3, "o").contains("@class = N'OPERATOR',\n    @type = N'NONE',")
    );
    let cat = Category {
        id: 120,
        class: 3,
        kind: 3,
        name: "ops".into(),
    };
    assert!(!cat.system());
    assert_eq!(category::rename_script(&cat, "ops"), None);
    let sql = category::rename_script(&cat, "on call").unwrap();
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_update_category @class = N'OPERATOR',\n    @name = N'ops',\n    @new_name = N'on call';"), "{sql}");
    let sql = category::drop_script(&cat);
    one_batch(&sql);
    assert!(sql.contains("msdb.dbo.sp_delete_category @class = N'OPERATOR',\n    @name = N'ops';"));
    assert!(Category {
        id: 0,
        class: 1,
        kind: 1,
        name: "[Uncategorized (Local)]".into()
    }
    .system());
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

/// Every label is Spanish text written where the extractor finds it (a
/// literal in the constructor's call): none is empty or a key.
#[test]
fn form_labels_are_written_out() {
    let cat = catalog();
    let mut all: Vec<FormField> = Vec::new();
    all.extend(schedule::fields(false, 20261112));
    all.extend(job::step_fields(&cat, true, &[]));
    all.extend(operator::general_fields());
    all.extend(alert::general_fields(true, "ns"));
    for f in &all {
        assert!(!f.label.is_empty() && f.label != f.key, "{}", f.key);
        if let FormKind::Select { options } | FormKind::MultiSelect { options } = &f.kind {
            for (v, l) in options {
                assert!(!v.is_empty() && !l.is_empty(), "{}: {v}", f.key);
            }
        }
    }
    // The extractor's patterns, as scripts/i18n/extract_backend.py reads
    // them: every label of these files comes from a literal.
    for (file, src) in [
        ("job.rs", include_str!("job.rs")),
        ("schedule.rs", include_str!("schedule.rs")),
        ("operator.rs", include_str!("operator.rs")),
        ("alert.rs", include_str!("alert.rs")),
        ("proxy.rs", include_str!("proxy.rs")),
        ("category.rs", include_str!("category.rs")),
        ("mod.rs", include_str!("mod.rs")),
    ] {
        for ctor in [
            "FormField::new(",
            "FormSection::new(",
            "FormList::new(",
            "SchedulerKind::new(",
        ] {
            let mut rest = src;
            while let Some(i) = rest.find(ctor) {
                rest = &rest[i + ctor.len()..];
                let args = rest.trim_start();
                let shown = &args[..args.len().min(60)];
                assert!(args.starts_with('"'), "{file}: {ctor}{shown}");
                // The label, the second argument, is a literal too.
                let after_key = &args[1..];
                let end = after_key.find('"').unwrap();
                let label = after_key[end + 1..].trim_start();
                assert!(label.starts_with(','), "{file}: {ctor}{shown}");
                assert!(
                    label[1..].trim_start().starts_with('"'),
                    "{file}: {ctor}{shown}"
                );
            }
        }
    }
}

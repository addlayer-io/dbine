use super::stream::{self, build_new, build_old, parse_new, parse_old};
use super::*;
use serde_json::json;

const OLD: &str = "3.3.6.13";

/// The script's "⚠" lines, as the app lists them above it.
fn warnings(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|l| l.strip_prefix("-- ⚠ "))
        .map(str::to_string)
        .collect()
}

fn old_ctx() -> Ctx {
    ctx_of(OLD, false)
}

fn new_ctx() -> Ctx {
    ctx_of("3.4.3.9.community", true)
}

fn values(v: Value) -> FormValues {
    let mut out = FormValues::default();
    for (k, x) in v.as_object().unwrap() {
        if k == "notify_urls" {
            out.lists.insert(
                k.clone(),
                x.as_array()
                    .unwrap()
                    .iter()
                    .map(|u| BTreeMap::from([("url".to_string(), u.clone())]))
                    .collect(),
            );
        } else {
            out.values.insert(k.clone(), x.clone());
        }
    }
    out
}

fn stream_row(v: Value) -> Stream {
    let rows = vec![v.as_object().unwrap().clone()];
    jobs::streams(&rows).remove(0)
}

fn sqls(p: &Plan) -> Vec<&str> {
    p.stmts.iter().map(|s| s.sql.as_str()).collect()
}

// ------------------------------------------------------------------ tokens

#[test]
fn the_lexer_keeps_clauses_whole() {
    let t = stream::lex("into db.`Out 8` (ts, c) as select 'a;b' from t").unwrap();
    assert_eq!(t.len(), 8);
    assert_eq!(t[1].t, stream::T::Word);
    assert_eq!(t[2].t, stream::T::Group);
    for bad in [
        "select 1; drop database x",
        "select 1 -- x",
        "select /* x */ 1",
        "select (1",
        "select 1)",
        "select 'a",
        "select # x",
    ] {
        assert!(stream::lex(bad).is_none(), "{bad}");
    }
    assert_eq!(
        stream::split_top("a, f(b, c), 'x, y'", b','),
        ["a", "f(b, c)", "'x, y'"]
    );
    assert_eq!(stream::name_parts("`db`.`s 8`"), ["db", "s 8"]);
    assert_eq!(stream::name_parts("db.s1"), ["db", "s1"]);
}

#[test]
fn checks_on_names_durations_tables_and_expressions() {
    assert_eq!(stream::dur("10S", "x").unwrap(), "10s");
    assert!(stream::dur("1n", "x").is_ok() && stream::dur("500a", "x").is_ok());
    for bad in ["", "s", "10", "10x", "1h30m", "10s)"] {
        assert!(stream::dur(bad, "x").is_err(), "{bad}");
    }
    assert!(stream::table("db.meters", "x").is_ok());
    assert!(stream::table("`db`.`Out 8`", "x").is_ok());
    assert!(stream::table("meters", "x").is_ok());
    for bad in [
        "a.b.c",
        "db.t; drop database db",
        "db.`t",
        "1t",
        "db.(t)",
        "a b",
        "",
    ] {
        assert!(stream::table(bad, "x").is_err(), "{bad}");
    }
    assert!(stream::fragment("concat('a;', loc)", "x").is_ok());
    for bad in ["loc) as select 1", "x; drop database db", "x -- y", "'x"] {
        assert!(stream::fragment(bad, "x").is_err(), "{bad}");
    }
    assert!(stream::plain("a`b", "x").is_err() && stream::plain("a\nb", "x").is_err());
    assert!(stream::plain("s 8", "x").is_ok());
}

#[test]
fn versions_decide_what_the_form_has() {
    let c = ctx_of("3.0.7.1", false);
    assert!(!c.suspend && !c.notify && !c.new_engine);
    let c = ctx_of("3.3.5.0", false);
    assert!(c.suspend && !c.notify);
    let c = old_ctx();
    assert!(c.suspend && c.notify);
    assert!(new_ctx().notify && new_ctx().suspend);
    let i = info_of(&ctx_of("3.0.7.1", false));
    assert_eq!(i.kinds[0].key, "job");
    assert!(i.kinds[0].can_create && i.notes[0].contains("PAUSE"));
    assert!(info_of(&old_ctx()).notes.is_empty());
    assert!(check_kind("job").is_ok() && check_kind("pipe").is_err());
}

// --------------------------------------------------------------- old engine

const OLD_S1: &str = "create stream if not exists dbine_test_sched_s1 trigger max_delay 5s watermark 10s ignore expired 0 delete_mark 1d fill_history 1 ignore update 1 into dbine_test_sched.agg1 (wstart, c) tags (loc varchar(16)) subtable(concat('agg_', loc)) as select _wstart, count(*) from dbine_test_sched.meters partition by loc interval(10s)";
const OLD_S4: &str = "create stream dbine_test_sched_s4 trigger window_close into dbine_test_sched.agg4 as select _wstart, count(*) from dbine_test_sched.meters interval(10s) notify('ws://localhost:12345/Notify') on ('WINDOW_OPEN', 'WINDOW_CLOSE') notify_history 1 on_failure pause";
const OLD_S6: &str = "create stream `dbine_test_sched_S6` into dbine_test_sched.`Agg6` tags (`Loc` varchar(16)) subtable(concat('AGG_', `Loc`)) as select _wstart, count(*) as `Cnt` from dbine_test_sched.meters where loc <> 'X Y' partition by loc as `Loc` interval(10s)";
const OLD_S9: &str = "create stream dbine_test_sched_s9 trigger continuous_window_close recalculate 11m ignore update 1 into dbine_test_sched.agg9 as select _wstart, count(*) from dbine_test_sched.meters partition by tbname interval(10s)";
const OLD_S5: &str = "create stream dbine_test_sched_s5 fill_history 1 async into dbine_test_sched.agg5 as select _wstart, count(*) from dbine_test_sched.meters interval(10s)";

#[test]
fn old_statements_read_into_the_form() {
    let v = parse_old(OLD_S1).unwrap().values;
    for (k, x) in [
        ("name", "dbine_test_sched_s1"),
        ("trigger", "max_delay"),
        ("max_delay", "5s"),
        ("watermark", "10s"),
        ("ignore_expired", "0"),
        ("delete_mark", "1d"),
        ("fill_history", "1"),
        ("ignore_update", "1"),
        ("into", "dbine_test_sched.agg1"),
        ("columns", "wstart, c"),
        ("tags", "loc varchar(16)"),
        ("subtable", "concat('agg_', loc)"),
        (
            "query",
            "select _wstart, count(*) from dbine_test_sched.meters partition by loc interval(10s)",
        ),
    ] {
        assert_eq!(v[k], json!(x), "{k}");
    }
    let f = parse_old(OLD_S4).unwrap();
    assert_eq!(
        f.values["notify_events"],
        json!(["window_open", "window_close"])
    );
    assert_eq!(f.values["notify_history"], json!("1"));
    assert_eq!(f.values["on_failure"], json!("pause"));
    assert_eq!(
        f.lists["notify_urls"][0]["url"],
        json!("ws://localhost:12345/Notify")
    );
    assert!(f.values["query"]
        .as_str()
        .unwrap()
        .ends_with("interval(10s)"));
    let v = parse_old(OLD_S6).unwrap().values;
    assert_eq!(v["name"], json!("dbine_test_sched_S6"));
    assert_eq!(v["into"], json!("dbine_test_sched.`Agg6`"));
    assert!(!v.contains_key("trigger"));
    let v = parse_old(OLD_S9).unwrap().values;
    assert_eq!(
        (v["trigger"].clone(), v["recalculate"].clone()),
        (json!("continuous_window_close"), json!("11m"))
    );
    let v = parse_old(OLD_S5).unwrap().values;
    assert_eq!(
        (v["fill_history"].clone(), v["fill_history_async"].clone()),
        (json!("1"), json!(true))
    );
    assert!(parse_old("create stream s into t").is_none());
    assert!(parse_old("create stream s trigger soon into t as select 1").is_none());
}

#[test]
fn old_statements_round_trip() {
    for sql in [OLD_S1, OLD_S4, OLD_S6, OLD_S9, OLD_S5] {
        let mut v = parse_old(sql).unwrap();
        v.values
            .insert("database".into(), json!("dbine_test_sched"));
        let b = build_old(&v, true, false).unwrap();
        let mut again = parse_old(&b.sql).unwrap_or_else(|| panic!("{}", b.sql));
        again
            .values
            .insert("database".into(), json!("dbine_test_sched"));
        assert_eq!(build_old(&again, true, false).unwrap(), b, "{sql}");
        assert_eq!(b.db, "dbine_test_sched");
    }
    let mut v = parse_old(OLD_S1).unwrap();
    v.values.insert("database".into(), json!("db"));
    assert_eq!(
        build_old(&v, true, false).unwrap().sql,
        "CREATE STREAM `dbine_test_sched_s1`\n  TRIGGER MAX_DELAY 5s WATERMARK 10s IGNORE EXPIRED 0 DELETE_MARK 1d FILL_HISTORY 1 IGNORE UPDATE 1\n  INTO dbine_test_sched.agg1 (wstart, c)\n  TAGS (loc varchar(16))\n  SUBTABLE(concat('agg_', loc))\n  AS\nselect _wstart, count(*) from dbine_test_sched.meters partition by loc interval(10s)"
    );
}

#[test]
fn old_statement_checks() {
    let base = json!({"database": "db", "name": "s", "into": "db.t", "query": "select _wstart, count(*) from db.m interval(1m)"});
    let with = |k: &str, x: Value| {
        let mut b = base.clone();
        b[k] = x;
        values(b)
    };
    let err = |v: FormValues| build_old(&v, true, false).unwrap_err().to_string();
    assert!(build_old(&values(base.clone()), true, false).is_ok());
    assert!(err(with("name", json!("a`b"))).contains("Nombre"));
    assert!(err(with("database", json!(""))).contains("base de datos"));
    assert!(err(with("into", json!(""))).contains("INTO"));
    assert!(err(with("into", json!("db.t; drop database db"))).contains("tabla"));
    assert!(err(with("query", json!(""))).contains("consulta"));
    assert!(err(with("query", json!("select 1; drop database db"))).contains("sola expresión"));
    assert!(err(with("trigger", json!("max_delay"))).contains("demora"));
    assert!(err(with("trigger", json!("soon"))).contains("disparo"));
    assert!(err(with("watermark", json!("10"))).contains("duración"));
    assert!(err(with("ignore_expired", json!("2"))).contains("0 ni 1"));
    assert!(err(with("tags", json!("loc varchar(16)) as select 1"))).contains("sola expresión"));
    assert!(err(with("notify_events", json!(["window_close"]))).contains("URL"));
    let mut v = with("notify_events", json!(["window_close"]));
    v.lists.insert(
        "notify_urls".into(),
        vec![BTreeMap::from([(
            "url".to_string(),
            json!("ws://h/x?k='1'"),
        )])],
    );
    let b = build_old(&v, true, false).unwrap();
    assert!(
        b.sql
            .ends_with("\n  NOTIFY('ws://h/x?k=''1''') ON ('WINDOW_CLOSE')"),
        "{}",
        b.sql
    );
    assert!(build_old(&v, false, false)
        .unwrap_err()
        .to_string()
        .contains("3.3.6"));
    let mut v = values(base);
    v.lists.insert(
        "notify_urls".into(),
        vec![BTreeMap::from([("url".to_string(), json!("ws://h"))])],
    );
    assert!(build_old(&v, true, false)
        .unwrap_err()
        .to_string()
        .contains("evento"));
}

fn old_stream(sql: &str, status: &str) -> Stream {
    stream_row(
        json!({"stream_name": "dbine_test_sched_s1", "sql": sql, "status": status,
        "source_db": "dbine_test_sched", "target_db": "dbine_test_sched", "trigger": "max delay",
        "create_time": "2026-11-12T21:49:04.273Z"}),
    )
}

#[test]
fn old_streams_change_by_dropping_and_creating() {
    let c = old_ctx();
    let st = old_stream(OLD_S1, "ready");
    let f = form_of(&c, Some(&st), &["dbine_test_sched".into()], None);
    assert!(!f.read_only);
    assert_eq!(f.values["database"], json!("dbine_test_sched"));
    assert_eq!(f.values["enabled"], json!(true));
    assert_eq!(f.values["status"], json!("ready"));
    assert!(f.notes.iter().any(|n| n.contains("ALTER STREAM")));
    assert!(f.sections.iter().any(|s| s.key == "notify"));
    assert_eq!(
        f.choices["database"],
        vec![(
            "dbine_test_sched".to_string(),
            "dbine_test_sched".to_string()
        )]
    );
    let back = || FormValues {
        values: f.values.clone(),
        lists: f
            .sections
            .iter()
            .flat_map(|s| s.lists.iter())
            .map(|l| (l.key.clone(), l.rows.clone()))
            .collect(),
    };
    // As it came: nothing.
    let p = plan(&c, Some(&st), &back(), Targets::default()).unwrap();
    assert!(p.stmts.is_empty(), "{}", p.script());
    // Paused only.
    let mut v = back();
    v.values.insert("enabled".into(), json!(false));
    let p = plan(&c, Some(&st), &v, Targets::default()).unwrap();
    assert_eq!(sqls(&p), ["PAUSE STREAM `dbine_test_sched_s1`"]);
    assert_eq!(
        p.stmts[0].wait,
        Some(("dbine_test_sched_s1".to_string(), false))
    );
    // Resumed, skipping what came meanwhile.
    let paused = old_stream(OLD_S1, "paused");
    let mut v = back();
    v.values
        .insert("resume_ignore_untreated".into(), json!(true));
    let p = plan(&c, Some(&paused), &v, Targets::default()).unwrap();
    assert_eq!(
        sqls(&p),
        ["RESUME STREAM IGNORE UNTREATED `dbine_test_sched_s1`"]
    );
    assert_eq!(
        warnings(&p.script()),
        ["Saltea lo que llegó mientras estuvo suspendido (IGNORE UNTREATED): esos datos no se procesan."]
    );
    // Resumed with the backlog: nothing to accept.
    v.values
        .insert("resume_ignore_untreated".into(), json!(false));
    let p = plan(&c, Some(&paused), &v, Targets::default()).unwrap();
    assert_eq!(sqls(&p), ["RESUME STREAM `dbine_test_sched_s1`"]);
    assert!(warnings(&p.script()).is_empty(), "{}", p.script());
    // Paused with the box ticked: nothing is skipped.
    v.values
        .insert("resume_ignore_untreated".into(), json!(true));
    v.values.insert("enabled".into(), json!(false));
    let p = plan(&c, Some(&st), &v, Targets::default()).unwrap();
    assert_eq!(sqls(&p), ["PAUSE STREAM `dbine_test_sched_s1`"]);
    assert!(warnings(&p.script()).is_empty(), "{}", p.script());
    // A new watermark: drop, create in the source database, the old
    // statement to put back (paused again if it was).
    let mut v = back();
    v.values.insert("watermark".into(), json!("20s"));
    v.values.insert("enabled".into(), json!(false));
    let p = plan(&c, Some(&paused), &v, Targets::default()).unwrap();
    assert_eq!(p.stmts[0].sql, "DROP STREAM `dbine_test_sched_s1`");
    assert!(p.stmts[1].sql.contains("WATERMARK 20s"));
    assert_eq!(p.stmts[1].db.as_deref(), Some("dbine_test_sched"));
    assert_eq!(p.stmts[2].sql, "PAUSE STREAM `dbine_test_sched_s1`");
    let r = p.recreate.as_ref().unwrap();
    assert_eq!((r.drop, r.create), (0, 1));
    assert!(
        r.restore[0].sql.starts_with(
            "CREATE STREAM `dbine_test_sched_s1`\n  TRIGGER MAX_DELAY 5s WATERMARK 10s"
        ),
        "{}",
        r.restore[0].sql
    );
    assert!(
        r.restore[0].sql.contains("\n  TAGS (loc)\n"),
        "its target exists now: {}",
        r.restore[0].sql
    );
    assert_eq!(r.restore[0].db.as_deref(), Some("dbine_test_sched"));
    assert_eq!(r.restore[1].sql, "PAUSE STREAM `dbine_test_sched_s1`");
    let script = p.script();
    assert!(
        script.starts_with(
            "-- Stream «dbine_test_sched_s1» de TDengine\n-- ⚠ TDengine no tiene ALTER STREAM"
        ),
        "{script}"
    );
    assert!(
        script
            .contains("DROP STREAM `dbine_test_sched_s1`;\nUSE `dbine_test_sched`;\nCREATE STREAM"),
        "{script}"
    );
    // Dropped and created again: its state goes; paused and kept so, the
    // new one runs until it's paused.
    assert_eq!(
        warnings(&script),
        [
            "TDengine no tiene ALTER STREAM: se borra el stream y se vuelve a crear.",
            "Se pierde su estado (ventanas abiertas, avance); la tabla de destino y sus filas quedan.",
            "El stream nuevo arranca al crearse: procesa datos hasta que DBine lo suspende, apenas sus tareas están listas.",
        ]
    );
    // Running and paused by this save: only the recreate.
    let p = plan(&c, Some(&st), &v, Targets::default()).unwrap();
    assert_eq!(warnings(&p.script()).len(), 2, "{}", p.script());
    // Running and kept so.
    v.values.insert("enabled".into(), json!(true));
    let p = plan(&c, Some(&st), &v, Targets::default()).unwrap();
    assert_eq!(warnings(&p.script()).len(), 2, "{}", p.script());
    // Paused, resumed or left alone: nothing to accept.
    let mut v = back();
    assert!(warnings(
        &plan(&c, Some(&st), &v, Targets::default())
            .unwrap()
            .script()
    )
    .is_empty());
    v.values.insert("enabled".into(), json!(false));
    assert!(warnings(
        &plan(&c, Some(&st), &v, Targets::default())
            .unwrap()
            .script()
    )
    .is_empty());
    assert!(warnings(&drop_plan(&st).script()).is_empty());
    // Without PAUSE (before 3.1): refused.
    let c30 = ctx_of("3.0.7.1", false);
    let mut v = back();
    v.values.insert("enabled".into(), json!(false));
    assert!(matches!(
        plan(&c30, Some(&st), &v, Targets::default()),
        Err(Error::Unsupported(_))
    ));
    assert!(form_of(&c30, Some(&st), &[], None).sections[0]
        .fields
        .iter()
        .any(|f| f.key == "enabled" && f.read_only));
    // Deleting.
    assert_eq!(
        drop_plan(&st).script(),
        "-- Borra el stream «dbine_test_sched_s1» de TDengine\nDROP STREAM `dbine_test_sched_s1`;\n"
    );
    // A statement DBine can't read.
    let odd = old_stream("create stream x weird", "ready");
    let f = form_of(&c, Some(&odd), &[], None);
    assert!(f.read_only && f.notes.iter().any(|n| n.contains("no pudo leer")));
    assert!(plan(&c, Some(&odd), &back(), Targets::default()).is_err());
}

#[test]
fn a_new_old_engine_stream() {
    let c = old_ctx();
    let f = form_of(&c, None, &["db1".into()], Some("db2"));
    assert_eq!(f.values["database"], json!("db2"));
    assert_eq!(f.choices["database"].len(), 2);
    assert!(f.sections[0]
        .fields
        .iter()
        .all(|f| f.key != "status" && f.key != "resume_ignore_untreated"));
    let v = values(
        json!({"database": "db", "name": "dbine_test_sched_s", "enabled": false,
        "trigger": "at_once", "into": "db.agg", "query": "select _wstart, count(*) from m interval(1m)"}),
    );
    let p = plan(&c, None, &v, Targets::default()).unwrap();
    assert_eq!(p.id, "dbine_test_sched_s");
    assert_eq!(
        p.script(),
        "-- Stream «dbine_test_sched_s» de TDengine\n-- Arranca al crearse: se suspende apenas sus tareas están listas.\nUSE `db`;\nCREATE STREAM `dbine_test_sched_s`\n  TRIGGER AT_ONCE\n  INTO db.agg\n  AS\nselect _wstart, count(*) from m interval(1m);\nPAUSE STREAM `dbine_test_sched_s`;\n"
    );
    assert!(warnings(&p.script()).is_empty());
}

// --------------------------------------------------------------- new engine

const NEW_S1: &str = "create stream if not exists dbine_test_sched.s1 interval(10s) sliding(10s) from dbine_test_sched.meters partition by tbname, loc STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0) | watermark(5s) | expired_time(1d) | ignore_disorder | fill_history('2026-01-01 00:00:00') | max_delay(3s)) notify('ws://localhost:12345/x') on (window_open|window_close) notify_options(notify_history) into dbine_test_sched.out1 output_subtable(concat('o_', `loc`)) (ts, c) tags (loc varchar(16) as loc) as select _twstart, count(*) from %%trows";
const NEW_S6: &str = "create stream dbine_test_sched.s6 count_window(10, 5, v) from dbine_test_sched.d1 STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0) | calc_notify_only | delete_output_table | fill_history_first) notify('ws://h:1/y') on (window_close) where c > 0 as select _twstart ts, count(*) c from %%trows";
const NEW_S4: &str = "create stream dbine_test_sched.s4 state_window(v) true_for(5s) from dbine_test_sched.d1 STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0)) into dbine_test_sched.out4 as select _twstart ts, count(*) c from %%trows";
const NEW_S3: &str = "create stream dbine_test_sched.s3 session(ts, 10s) from dbine_test_sched.d1 STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0) | delete_recalc | low_latency_calc) into dbine_test_sched.out3 as select _twstart ts, count(*) c from %%trows";
const NEW_S8: &str = "create stream dbine_test_sched.`s 8` period(1m) STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0) | expired_time(1h)) into dbine_test_sched.`Out 8` as select cast(_tlocaltime/1000000 as timestamp) ts, count(*) c from dbine_test_sched.meters where loc = 'A b'";
const NEW_S7: &str = "create stream dbine_test_sched.s7 interval(1m, 10s) sliding(30s) from dbine_test_sched.d1 STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0) | ignore_nodata_trigger | fill_history) into dbine_test_sched.out7 as select _twstart ts, count(*) c from %%trows";
const NEW_S12: &str = "create stream dbine_test_sched.s12 event_window(start with v > 10 end with v < 5) from dbine_test_sched.d1 STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0) | pre_filter(v > 0) | force_output) into dbine_test_sched.out12 as select _twstart ts, count(*) c from %%trows";
const NEW_S9: &str = "create stream dbine_test_sched.s9 sliding(30s, 1s) from dbine_test_sched.d1 STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0)) into dbine_test_sched.out9 as select _tcurrent_ts ts, count(*) c from dbine_test_sched.d1";
const NEW_S2: &str = "create stream dbine_test_sched.s2 period(1m, 5s) STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0) | event_type(window_open|window_close) | low_latency_calc | foo_option(3)) into dbine_test_sched.out2 as select cast(_tlocaltime/1000000 as timestamp) ts, count(*) c from dbine_test_sched.meters";

#[test]
fn new_statements_read_into_the_form() {
    let f = parse_new(NEW_S1).unwrap();
    let v = &f.values;
    for (k, x) in [
        ("database", json!("dbine_test_sched")),
        ("name", json!("s1")),
        ("trigger", json!("sliding")),
        ("interval", json!("10s")),
        ("sliding", json!("10s")),
        ("from_table", json!("dbine_test_sched.meters")),
        ("partition_by", json!("tbname, loc")),
        ("timezone", json!("Etc/UTC")),
        ("first_day_of_week", json!("0")),
        ("watermark", json!("5s")),
        ("expired_time", json!("1d")),
        ("ignore_disorder", json!(true)),
        ("fill", json!("fill_history")),
        ("fill_start", json!("2026-01-01 00:00:00")),
        ("max_delay", json!("3s")),
        ("notify_events", json!(["window_open", "window_close"])),
        ("notify_history", json!(true)),
        ("into", json!("dbine_test_sched.out1")),
        ("subtable", json!("concat('o_', `loc`)")),
        ("columns", json!("ts, c")),
        ("tags", json!("loc varchar(16) as loc")),
        ("query", json!("select _twstart, count(*) from %%trows")),
    ] {
        assert_eq!(v[k], x, "{k}");
    }
    assert_eq!(
        f.lists["notify_urls"][0]["url"],
        json!("ws://localhost:12345/x")
    );
    let v = parse_new(NEW_S6).unwrap().values;
    assert_eq!(
        (
            v["trigger"].clone(),
            v["count"].clone(),
            v["count_sliding"].clone(),
            v["count_cols"].clone()
        ),
        (json!("count"), json!("10"), json!("5"), json!("v"))
    );
    assert_eq!(v["notify_where"], json!("c > 0"));
    assert_eq!(v["fill"], json!("fill_history_first"));
    assert!(!v.contains_key("into"));
    let v = parse_new(NEW_S4).unwrap().values;
    assert_eq!(
        (v["state_expr"].clone(), v["true_for"].clone()),
        (json!("v"), json!("5s"))
    );
    let v = parse_new(NEW_S3).unwrap().values;
    assert_eq!(
        (v["session_col"].clone(), v["session_gap"].clone()),
        (json!("ts"), json!("10s"))
    );
    let v = parse_new(NEW_S8).unwrap().values;
    assert_eq!(
        (v["name"].clone(), v["period"].clone(), v["into"].clone()),
        (json!("s 8"), json!("1m"), json!("dbine_test_sched.`Out 8`"))
    );
    let v = parse_new(NEW_S7).unwrap().values;
    assert_eq!(
        (
            v["interval"].clone(),
            v["interval_offset"].clone(),
            v["sliding"].clone()
        ),
        (json!("1m"), json!("10s"), json!("30s"))
    );
    let v = parse_new(NEW_S12).unwrap().values;
    assert_eq!(
        (
            v["start_with"].clone(),
            v["end_with"].clone(),
            v["pre_filter"].clone()
        ),
        (json!("v > 10"), json!("v < 5"), json!("v > 0"))
    );
    let v = parse_new(NEW_S9).unwrap().values;
    assert_eq!(
        (v["sliding"].clone(), v["sliding_offset"].clone()),
        (json!("30s"), json!("1s"))
    );
    assert!(!v.contains_key("interval"));
    let v = parse_new(NEW_S2).unwrap().values;
    assert_eq!(v["period_offset"], json!("5s"));
    assert_eq!(v["event_type"], json!(["window_open", "window_close"]));
    assert_eq!(
        v["extra_options"],
        json!("foo_option(3)"),
        "an option DBine doesn't know is kept"
    );
    assert!(parse_new("create stream db.s soon(1m) as select 1").is_none());
}

#[test]
fn new_statements_round_trip() {
    for sql in [
        NEW_S1, NEW_S6, NEW_S4, NEW_S3, NEW_S8, NEW_S7, NEW_S12, NEW_S9, NEW_S2,
    ] {
        let v = parse_new(sql).unwrap();
        let b = build_new(&v).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let again = parse_new(&b.sql).unwrap_or_else(|| panic!("{}", b.sql));
        assert_eq!(build_new(&again).unwrap(), b, "{sql}");
        assert_eq!(b.db, "dbine_test_sched");
    }
    assert_eq!(
        build_new(&parse_new(NEW_S1).unwrap()).unwrap().sql,
        "CREATE STREAM `dbine_test_sched`.`s1`\n  INTERVAL(10s) SLIDING(10s)\n  FROM dbine_test_sched.meters\n  PARTITION BY tbname, loc\n  STREAM_OPTIONS(CONFIG(timezone = 'Etc/UTC', firstDayOfWeek = 0) | WATERMARK(5s) | EXPIRED_TIME(1d) | IGNORE_DISORDER | FILL_HISTORY('2026-01-01 00:00:00') | MAX_DELAY(3s))\n  NOTIFY('ws://localhost:12345/x') ON (WINDOW_OPEN|WINDOW_CLOSE) NOTIFY_OPTIONS(NOTIFY_HISTORY)\n  INTO dbine_test_sched.out1\n  OUTPUT_SUBTABLE(concat('o_', `loc`))\n  (ts, c)\n  TAGS (loc varchar(16) as loc)\n  AS\nselect _twstart, count(*) from %%trows"
    );
}

#[test]
fn new_statement_checks() {
    let base = json!({"database": "db", "name": "s", "trigger": "period", "period": "1m",
        "into": "db.t", "query": "select now, count(*) from db.m"});
    let with = |k: &str, x: Value| {
        let mut b = base.clone();
        b[k] = x;
        values(b)
    };
    let err = |v: FormValues| build_new(&v).unwrap_err().to_string();
    assert!(build_new(&values(base.clone())).is_ok());
    assert!(err(with("period", json!(""))).contains("PERIOD"));
    assert!(err(with("trigger", json!("cron"))).contains("disparador"));
    assert!(err(with("query", json!(""))).contains("consulta"));
    assert!(err(with("from_table", json!("a; drop database db"))).contains("tabla"));
    assert!(err(with("first_day_of_week", json!(9))).contains("6"));
    assert!(err(with("fill_start", json!("2025-01-01"))).contains("históricos"));
    assert!(err(with("notify_where", json!("c > 0"))).contains("URL"));
    assert!(err(with("event_type", json!(["window_middle"]))).contains("evento"));
    assert!(err(with("extra_options", json!("a || b"))).contains("|"));
    assert!(err(with("pre_filter", json!("v > 0) | calc_notify_only"))).contains("sola expresión"));
    let mut b = base.clone();
    b["trigger"] = json!("sliding");
    b["interval_offset"] = json!("1s");
    b["sliding"] = json!("1m");
    assert!(err(values(b)).contains("INTERVAL"));
    let mut b = base.clone();
    b.as_object_mut().unwrap().remove("into");
    b["columns"] = json!("ts, c");
    assert!(err(values(b)).contains("INTO"));
    // A notify-only stream needs no table nor query.
    let mut b = base;
    b.as_object_mut().unwrap().remove("into");
    b.as_object_mut().unwrap().remove("query");
    b["calc_notify_only"] = json!(true);
    b["notify_urls"] = json!(["ws://h:1/x"]);
    b["notify_events"] = json!(["window_close"]);
    b["fill"] = json!("fill_history");
    b["fill_start"] = json!("1700000000000");
    let s = build_new(&values(b)).unwrap().sql;
    assert!(s.ends_with("STREAM_OPTIONS(FILL_HISTORY(1700000000000) | CALC_NOTIFY_ONLY)\n  NOTIFY('ws://h:1/x') ON (WINDOW_CLOSE)"), "{s}");
}

fn new_stream(sql: &str, status: &str) -> Stream {
    stream_row(
        json!({"stream_name": "s1", "db_name": "dbine_test_sched", "sql": sql, "status": status,
        "create_time": "2026-11-12T16:25:52.589Z", "message": ""}),
    )
}

#[test]
fn new_streams_change_by_dropping_and_creating() {
    let c = new_ctx();
    let st = new_stream(NEW_S1, "Running");
    let f = form_of(&c, Some(&st), &[], None);
    assert!(!f.read_only);
    assert_eq!(f.values["database"], json!("dbine_test_sched"));
    assert_eq!(f.choices["database"][0].0, "dbine_test_sched");
    assert!(f.sections.iter().any(|s| s.key == "options"));
    let back = || FormValues {
        values: f.values.clone(),
        lists: f
            .sections
            .iter()
            .flat_map(|s| s.lists.iter())
            .map(|l| (l.key.clone(), l.rows.clone()))
            .collect(),
    };
    assert!(plan(&c, Some(&st), &back(), Targets::default())
        .unwrap()
        .stmts
        .is_empty());
    // Stopped only.
    let mut v = back();
    v.values.insert("enabled".into(), json!(false));
    let p = plan(&c, Some(&st), &v, Targets::default()).unwrap();
    assert_eq!(sqls(&p), ["STOP STREAM `dbine_test_sched`.`s1`"]);
    // Started again.
    let stopped = new_stream(NEW_S1, "Stopped");
    let p = plan(&c, Some(&stopped), &back(), Targets::default()).unwrap();
    assert_eq!(sqls(&p), ["START STREAM `dbine_test_sched`.`s1`"]);
    // A new trigger, and a new name: drop, create, the old one to put back.
    let mut v = back();
    v.values.insert("sliding".into(), json!("20s"));
    v.values.insert("name".into(), json!("s1b"));
    let p = plan(&c, Some(&st), &v, Targets::default()).unwrap();
    assert_eq!(p.id, "dbine_test_sched.s1b");
    assert_eq!(p.stmts[0].sql, "DROP STREAM `dbine_test_sched`.`s1`");
    assert!(p.stmts[1]
        .sql
        .starts_with("CREATE STREAM `dbine_test_sched`.`s1b`\n  INTERVAL(10s) SLIDING(20s)"));
    let r = p.recreate.as_ref().unwrap();
    assert!(r.restore[0]
        .sql
        .starts_with("CREATE STREAM `dbine_test_sched`.`s1`\n  INTERVAL(10s) SLIDING(10s)"));
    assert!(
        r.restore[0]
            .sql
            .contains("\n  TAGS (loc varchar(16) as loc)\n"),
        "the new engine keeps the definitions: {}",
        r.restore[0].sql
    );
    assert_eq!(r.restore[0].db.as_deref(), Some("dbine_test_sched"));
    // A new one, stopped at once.
    let v = values(
        json!({"database": "db", "name": "n", "enabled": false, "trigger": "period", "period": "1m",
        "into": "db.t", "query": "select now, count(*) from db.m"}),
    );
    let p = plan(&c, None, &v, Targets::default()).unwrap();
    assert_eq!(p.id, "db.n");
    assert_eq!(sqls(&p)[1], "STOP STREAM `db`.`n`");
    assert_eq!(p.stmts[1].wait, Some(("db.n".to_string(), false)));
    assert_eq!(
        drop_plan(&st).stmts[0].sql,
        "DROP STREAM `dbine_test_sched`.`s1`"
    );
}

#[test]
fn tags_go_by_name_into_an_existing_table() {
    let c = old_ctx();
    let st = old_stream(OLD_S1, "ready");
    let mut v = current_values(&c, &st).unwrap();
    v.values.insert(
        "tags".into(),
        json!("loc varchar(16), `Zone` nchar(8) COMMENT 'z'"),
    );
    let both = Targets {
        old: true,
        new: true,
    };
    let p = plan(&c, Some(&st), &v, both).unwrap();
    assert!(
        p.stmts[1].sql.contains("\n  TAGS (loc, `Zone`)\n"),
        "{}",
        p.stmts[1].sql
    );
    assert!(p.script().contains("TAGS lleva solo los nombres"));
    // Unchanged against an existing target: nothing to do.
    let v = current_values(&c, &st).unwrap();
    assert!(plan(&c, Some(&st), &v, both).unwrap().stmts.is_empty());
    // A new stream into an existing table.
    let mut n = v.clone();
    n.values.insert("name".into(), json!("other"));
    let p = plan(
        &c,
        None,
        &n,
        Targets {
            old: false,
            new: true,
        },
    )
    .unwrap();
    assert!(p.stmts[0].sql.contains("TAGS (loc)"));
    // Where INTO points, resolved against the database.
    assert_eq!(
        stream::target_table(&v),
        Some(("dbine_test_sched".into(), "agg1".into()))
    );
    let t = values(json!({"database": "db", "into": "`Other`.`T 1`"}));
    assert_eq!(
        stream::target_table(&t),
        Some(("Other".into(), "T 1".into()))
    );
    let t = values(json!({"database": "db", "into": "AGG"}));
    assert_eq!(stream::target_table(&t), Some(("db".into(), "agg".into())));
}

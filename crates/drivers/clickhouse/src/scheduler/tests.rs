//! Unit tests of the scheduler's parsers, forms, checks and scripts (no
//! server). The `SHOW CREATE` texts are what ClickHouse 26.9 and Timeplus
//! Proton 3.0 print.

use super::lex::{self, closed, normalized, same_sql};
use super::refresh::{self, parse_interval, parse_view, Unit, ViewDef};
use super::task::{self, parse_task, proton_interval, BadInterval, Schedule, TaskSpec};
use super::*;
use dbine_driver::FormKind;
use serde_json::json;

fn ctx(version: &str) -> Ctx {
    Ctx {
        version: version.into(),
        readonly: false,
        available: true,
    }
}

fn new_ch() -> Ctx {
    ctx("26.9.2.8")
}

fn choices() -> refresh::Choices {
    let p = |a: &str, b: &str| (a.to_string(), b.to_string());
    refresh::Choices {
        databases: vec![p("db", "Atomic"), p("x y", "Atomic"), p("old", "Ordinary")],
        refreshable: vec![p("db", "a"), p("db", "mv"), p("x y", "b")],
        tables: vec![p("db", "dst"), p("db", "src")],
        users: Some(vec!["ana".into(), "dbine".into()]),
    }
}

fn fv(values: Value) -> FormValues {
    FormValues {
        values: serde_json::from_value(values).unwrap(),
        lists: BTreeMap::new(),
    }
}

/// The statements a query tab runs for the plan's script: its `SET`s, its
/// `USE` and its own.
fn in_editor(p: &Plan) -> usize {
    p.settings.len() + usize::from(p.database.is_some()) + p.statements.len()
}

fn err_of<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(Error::Query(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A new view's values with these changes.
fn view_values(changes: Value) -> FormValues {
    let mut v = json!({
        "database": "db",
        "name": "dbine_test_sched_mv",
        "enabled": true,
        "mode": "every",
        "interval": "1 HOUR",
        "target_mode": "inner",
        "engine": "MergeTree ORDER BY c",
        "sql_security": "definer",
        "definer": "CURRENT_USER",
        "query": "SELECT count() AS c FROM db.src",
    });
    for (k, x) in changes.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v)
}

const FULL: &str = "CREATE MATERIALIZED VIEW dbine_test_sched_x.c\nREFRESH EVERY 1 MONTH OFFSET 5 DAY 2 HOUR RANDOMIZE FOR 1 HOUR 30 MINUTE SETTINGS refresh_retries = 3, refresh_retry_initial_backoff_ms = 200, refresh_retry_max_backoff_ms = 5000\n(\n    `c` UInt64\n)\nENGINE = MergeTree\nORDER BY c\nSETTINGS index_granularity = 8192\nDEFINER = dbine SQL SECURITY DEFINER\nCOMMENT 'hola'\nAS SELECT count() AS c\nFROM dbine_test_sched_x.src";

const APPEND_TO: &str = "CREATE MATERIALIZED VIEW dbine_test_sched_x.b\nREFRESH AFTER 30 SECOND DEPENDS ON dbine_test_sched_x.a APPEND TO dbine_test_sched_x.dst\n(\n    `c` UInt64\n)\nDEFINER = dbine SQL SECURITY DEFINER\nAS SELECT count() AS c\nFROM dbine_test_sched_x.src";

const QUOTED: &str = "CREATE MATERIALIZED VIEW dbine_test_sched_y.c\nREFRESH EVERY 1 HOUR DEPENDS ON dbine_test_sched_y.`a b`, `x y`.`b`\n(\n    `c` UInt64,\n    `d` String DEFAULT 'x'\n)\nENGINE = MergeTree\nORDER BY c\nSETTINGS index_granularity = 8192\nSQL SECURITY NONE\nCOMMENT 'it\\'s \\\\ ok'\nAS SELECT count() AS c\nFROM dbine_test_sched_y.src";

// ---------------------------------------------------------------------------
// The lexer
// ---------------------------------------------------------------------------

#[test]
fn fragments_must_be_closed() {
    assert!(closed("MergeTree ORDER BY (a, b) SETTINGS x = 'a;b'"));
    assert!(closed("SELECT 1 -- a comment; with a semicolon\nFROM t"));
    assert!(closed(
        "SELECT $x$ ; $x$, `a;b`, \"c\" /* ; /* nested */ */"
    ));
    for bad in [
        "MergeTree; DROP TABLE x",
        "MergeTree ORDER BY (a",
        "MergeTree ORDER BY a)",
        "SELECT 'open",
        "SELECT `open",
        "SELECT 1 /* open",
        "SELECT $x$ open",
    ] {
        assert!(!closed(bad), "{bad}");
    }
}

#[test]
fn texts_compare_by_their_tokens() {
    assert!(same_sql(
        "SELECT count()  AS c\nFROM t",
        "SELECT count() AS c FROM t"
    ));
    assert!(!same_sql("SELECT 'a  b'", "SELECT 'a b'"));
    assert_eq!(normalized("a\n  b").as_deref(), Some("a b"));
    assert_eq!(lex::unquote("'it\\'s \\\\ ok'"), "it's \\ ok");
    assert_eq!(lex::unquote("`a``b`"), "a`b");
    assert_eq!(lex::unquote("'a\\nb\\x41'"), "a\nbA");
    assert_eq!(lex::unquote("$t$x ' y$t$"), "x ' y");
    assert_eq!(
        lex::dedent("\n    `c` UInt64,\n    `d` Date\n"),
        "`c` UInt64,\n`d` Date"
    );
    assert_eq!(lex::trim_query("SELECT 1 ;; \n"), "SELECT 1");
}

// ---------------------------------------------------------------------------
// ClickHouse: intervals and the definition
// ---------------------------------------------------------------------------

#[test]
fn clickhouse_intervals() {
    assert_eq!(
        parse_interval("1 hour 30 minutes"),
        Some(vec![(1, Unit::Hour), (30, Unit::Minute)])
    );
    assert_eq!(
        refresh::interval_text(&parse_interval("2 WEEKS").unwrap()),
        "2 WEEK"
    );
    for bad in [
        "",
        "1",
        "HOUR",
        "1h",
        "1 FORTNIGHT",
        "-1 HOUR",
        "1.5 HOUR",
        "1 HOUR 2",
    ] {
        assert_eq!(parse_interval(bad), None, "{bad}");
    }
}

#[test]
fn the_definition_is_read_whole() {
    let d = parse_view(FULL).unwrap();
    assert_eq!(
        (d.database.as_str(), d.name.as_str()),
        ("dbine_test_sched_x", "c")
    );
    assert!(d.every);
    assert_eq!(d.interval, vec![(1, Unit::Month)]);
    assert_eq!(d.offset, Some(vec![(5, Unit::Day), (2, Unit::Hour)]));
    assert_eq!(d.randomize, Some(vec![(1, Unit::Hour), (30, Unit::Minute)]));
    assert_eq!(
        d.settings,
        vec![
            ("refresh_retries".to_string(), "3".to_string()),
            ("refresh_retry_initial_backoff_ms".into(), "200".into()),
            ("refresh_retry_max_backoff_ms".into(), "5000".into()),
        ]
    );
    assert_eq!(d.columns.as_deref(), Some("`c` UInt64"));
    assert_eq!(
        d.engine.as_deref(),
        Some("MergeTree\nORDER BY c\nSETTINGS index_granularity = 8192")
    );
    assert_eq!(d.definer.as_deref(), Some("dbine"));
    assert_eq!(d.security.as_deref(), Some("DEFINER"));
    assert_eq!(d.comment.as_deref(), Some("hola"));
    assert_eq!(d.query, "SELECT count() AS c\nFROM dbine_test_sched_x.src");
    assert!(d.to.is_none() && !d.append && d.depends_on.is_empty());

    let d = parse_view(APPEND_TO).unwrap();
    assert!(!d.every && d.append);
    assert_eq!(d.interval, vec![(30, Unit::Second)]);
    assert_eq!(
        d.depends_on,
        vec![("dbine_test_sched_x".to_string(), "a".to_string())]
    );
    assert_eq!(
        d.to,
        Some(("dbine_test_sched_x".to_string(), "dst".to_string()))
    );
    assert!(d.engine.is_none());

    let d = parse_view(QUOTED).unwrap();
    assert_eq!(
        d.depends_on,
        vec![
            ("dbine_test_sched_y".to_string(), "a b".to_string()),
            ("x y".to_string(), "b".to_string()),
        ]
    );
    assert_eq!(
        d.columns.as_deref(),
        Some("`c` UInt64,\n`d` String DEFAULT 'x'")
    );
    assert_eq!(d.security.as_deref(), Some("NONE"));
    assert_eq!(d.definer, None);
    assert_eq!(d.comment.as_deref(), Some("it's \\ ok"));
}

#[test]
fn older_servers_and_odd_settings_are_read() {
    // Before 24.x: no DEFINER, the comment after the query.
    let d = parse_view(
        "CREATE MATERIALIZED VIEW db.mv REFRESH EVERY 1 HOUR (`c` UInt64) ENGINE = MergeTree ORDER BY c AS SELECT 1 AS c COMMENT 'it\\'s'",
    )
    .unwrap();
    assert_eq!(d.query, "SELECT 1 AS c");
    assert_eq!(d.comment.as_deref(), Some("it's"));
    assert!(d.security.is_none());
    let d = parse_view(
        "CREATE MATERIALIZED VIEW db.mv REFRESH AFTER 1 DAY 3 HOUR SETTINGS refresh_retries = -1, all_replicas = 1, future_one = 'x' (`c` UInt64) ENGINE = Memory AS SELECT 1",
    )
    .unwrap();
    assert_eq!(
        d.settings,
        vec![
            ("refresh_retries".to_string(), "-1".to_string()),
            ("all_replicas".into(), "1".into()),
            ("future_one".into(), "'x'".into()),
        ]
    );
    // A view named like a keyword.
    let d = parse_view(
        "CREATE MATERIALIZED VIEW refresh.`AS` REFRESH EVERY 1 HOUR TO refresh.t AS SELECT 1",
    )
    .unwrap();
    assert_eq!((d.database.as_str(), d.name.as_str()), ("refresh", "AS"));
    assert_eq!(d.to, Some(("refresh".into(), "t".into())));
    // Not a refreshable view.
    assert!(parse_view("CREATE MATERIALIZED VIEW db.mv TO db.t AS SELECT 1").is_none());
    assert!(parse_view("CREATE TABLE db.t (a UInt8) ENGINE = Memory").is_none());
}

#[test]
fn the_definition_is_written_back() {
    let mut d = parse_view(QUOTED).unwrap();
    d.name = "v`1".into();
    d.offset = Some(vec![(10, Unit::Minute)]);
    d.settings = vec![("refresh_retries".into(), "2".into())];
    let sql = refresh::create_sql(&d, false, true);
    assert_eq!(
        sql,
        "CREATE MATERIALIZED VIEW `dbine_test_sched_y`.`v``1`\n\
         REFRESH EVERY 1 HOUR OFFSET 10 MINUTE DEPENDS ON `dbine_test_sched_y`.`a b`, `x y`.`b` SETTINGS refresh_retries = 2\n\
         (\n    `c` UInt64,\n    `d` String DEFAULT 'x'\n)\n\
         ENGINE = MergeTree\nORDER BY c\nSETTINGS index_granularity = 8192\n\
         SQL SECURITY NONE\n\
         AS\nSELECT count() AS c\nFROM dbine_test_sched_y.src\n\
         COMMENT 'it\\'s \\\\ ok'"
    );
    one_statement(&sql).unwrap();
    // Read back the same.
    let back = parse_view(&sql).unwrap();
    assert_eq!(back, d);

    let d = parse_view(APPEND_TO).unwrap();
    let sql = refresh::create_sql(&d, true, true);
    assert!(sql.starts_with("CREATE OR REPLACE MATERIALIZED VIEW `dbine_test_sched_x`.`b`\nREFRESH AFTER 30 SECOND DEPENDS ON `dbine_test_sched_x`.`a` APPEND\nTO `dbine_test_sched_x`.`dst`\n"), "{sql}");
    assert!(
        sql.contains("\nDEFINER = `dbine` SQL SECURITY DEFINER\n"),
        "{sql}"
    );
    // AFTER has no OFFSET; before 24.2 no SQL SECURITY.
    let mut d = d;
    d.offset = Some(vec![(1, Unit::Minute)]);
    let sql = refresh::create_sql(&d, false, false);
    assert!(
        !sql.contains("OFFSET") && !sql.contains("SECURITY"),
        "{sql}"
    );
}

// ---------------------------------------------------------------------------
// ClickHouse: the form and its checks
// ---------------------------------------------------------------------------

#[test]
fn the_form_has_every_part_of_the_view() {
    let mut d = parse_view(FULL).unwrap();
    d.database = "db".into();
    d.name = "mv".into();
    let f = refresh::build_form(&d, false, true, &new_ch(), &choices());
    let sections: Vec<&str> = f.sections.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(
        sections,
        ["general", "schedule", "retries", "target", "security", "query"]
    );
    let field = |k: &str| {
        f.sections
            .iter()
            .flat_map(|s| s.fields.iter())
            .find(|x| x.key == k)
            .unwrap_or_else(|| panic!("{k}"))
    };
    assert_eq!(
        field("offset").when,
        Some(("mode".into(), vec!["every".into()]))
    );
    assert_eq!(
        field("engine").when,
        Some(("target_mode".into(), vec!["inner".into()]))
    );
    assert!(matches!(
        field("depends_on").kind,
        FormKind::Ref { multiple: true, .. }
    ));
    assert!(matches!(&field("query").kind, FormKind::Code { language } if language == "sql"));
    assert!(f
        .sections
        .iter()
        .all(|s| s.fields.iter().all(|x| x.key != "empty")));
    assert_eq!(f.values["enabled"], json!(false));
    assert_eq!(f.values["interval"], json!("1 MONTH"));
    assert_eq!(f.values["offset"], json!("5 DAY 2 HOUR"));
    assert_eq!(f.values["refresh_retries"], json!(3));
    assert_eq!(f.values["sql_security"], json!("definer"));
    assert_eq!(f.values["definer"], json!("dbine"));
    // It can't depend on itself.
    let deps: Vec<&str> = f.choices["refreshable"]
        .iter()
        .map(|(v, _)| v.as_str())
        .collect();
    assert_eq!(deps, ["`db`.`a`", "`x y`.`b`"]);
    assert_eq!(f.choices["user"][0].0, "CURRENT_USER");
    assert!(f.notes.iter().any(|n| n.contains("CREATE OR REPLACE")));

    // Before 24.2 there's no security; before 24.10 a note.
    let f = refresh::build_form(&d, true, false, &ctx("24.1.3.31"), &choices());
    assert!(f.sections.iter().all(|s| s.key != "security"));
    assert!(f
        .sections
        .iter()
        .any(|s| s.fields.iter().any(|x| x.key == "empty")));
    assert!(f.notes.iter().any(|n| n.contains("experimentales")));
}

#[test]
fn values_read_back_into_the_same_view() {
    let ch = choices();
    for text in [FULL, APPEND_TO, QUOTED] {
        let mut d = parse_view(text).unwrap();
        d.database = "db".into();
        d.name = "mv".into();
        d.depends_on.retain(|p| ch.refreshable.contains(p));
        if d.to.is_some() {
            d.to = Some(("db".into(), "dst".into()));
        }
        let form = refresh::build_form(&d, true, true, &new_ch(), &ch);
        let values = FormValues {
            values: form.values.clone(),
            lists: BTreeMap::new(),
        };
        let (w, enabled) = refresh::wanted(&values, &new_ch(), &ch, Some(&d)).unwrap();
        assert!(enabled);
        assert_eq!(w, d, "{text}");
        let p = refresh::plan_edit(&d, true, &w, true, &new_ch(), true).unwrap();
        assert!(p.statements.is_empty(), "{p:?}");
        assert_eq!(p.script(), "-- db.mv: no hay cambios que guardar.\n");
    }
}

#[test]
fn bad_values_are_refused_before_any_sql() {
    let c = new_ch();
    let ch = choices();
    let refuse = |changes: Value, needle: &str| {
        let m = err_of(refresh::wanted(
            &view_values(changes.clone()),
            &c,
            &ch,
            None,
        ));
        assert!(m.contains(needle), "{changes}: {m}");
    };
    refuse(json!({"name": ""}), "«Nombre» es obligatorio");
    refuse(json!({"name": "a\nb"}), "no es un nombre válido");
    refuse(json!({"database": "nope"}), "no existe");
    refuse(json!({"mode": "sometimes"}), "no es un modo de refresco");
    refuse(json!({"interval": ""}), "«Intervalo» es obligatorio");
    refuse(
        json!({"interval": "1h"}),
        "no es un intervalo de ClickHouse",
    );
    refuse(json!({"interval": "0 HOUR"}), "mayor que cero");
    refuse(json!({"interval": "1 MONTH 2 DAY"}), "no mezcla meses");
    refuse(json!({"offset": "2 HOUR"}), "menor que el intervalo");
    refuse(json!({"offset": "60 MINUTE"}), "menor que el intervalo");
    refuse(json!({"randomize": "soon"}), "RANDOMIZE FOR");
    refuse(
        json!({"depends_on": ["`db`.`nope`"]}),
        "no es una vista refrescable",
    );
    refuse(
        json!({"depends_on": ["db.a"]}),
        "no es una vista refrescable",
    );
    refuse(
        json!({"name": "mv", "depends_on": ["`db`.`mv`"]}),
        "de sí misma",
    );
    refuse(json!({"refresh_retries": -2}), "entre -1 y");
    refuse(
        json!({"refresh_retry_max_backoff_ms": "x"}),
        "número entero",
    );
    refuse(json!({"other_settings": "a = 1; DROP"}), "no se entiende");
    refuse(
        json!({"other_settings": "refresh_retries = 1"}),
        "su propio campo",
    );
    refuse(json!({"other_settings": "a = 1, a = 2"}), "dos veces");
    refuse(
        json!({"target_mode": "to"}),
        "«Tabla destino» es obligatorio",
    );
    refuse(
        json!({"target_mode": "to", "target": "`other`.`t`"}),
        "no es una tabla",
    );
    refuse(json!({"target_mode": "x"}), "no es un destino");
    refuse(json!({"engine": "Memory; DROP DATABASE db"}), "«Motor»");
    refuse(json!({"columns": "(a UInt8"}), "«Columnas»");
    refuse(json!({"sql_security": "invoker"}), "SQL SECURITY");
    refuse(json!({"definer": "mallory"}), "no existe en este servidor");
    refuse(json!({"query": "  ;  "}), "«Consulta» es obligatorio");
    refuse(
        json!({"query": "SELECT 1; DROP TABLE db.src"}),
        "«Consulta»",
    );
    refuse(json!({"query": "SELECT 1 /* open"}), "«Consulta»");
    // A user list the login can't read: any name, quoted.
    let mut ch2 = choices();
    ch2.users = None;
    let (w, _) = refresh::wanted(&view_values(json!({"definer": "x`y"})), &c, &ch2, None).unwrap();
    assert!(refresh::create_sql(&w, false, true).contains("DEFINER = `x``y` SQL SECURITY DEFINER"));
}

#[test]
fn a_new_view_is_one_create() {
    let c = new_ch();
    let ch = choices();
    let (w, enabled) = refresh::wanted(
        &view_values(json!({
            "name": "dbine_test_sched_mv 'x'",
            "offset": "5 minutes",
            "randomize": "1 minute",
            "depends_on": ["`db`.`a`", "`x y`.`b`", "`db`.`a`"],
            "refresh_retries": 3,
            "all_replicas": true,
            "other_settings": "refresh_future = 'on', other = -1.5",
            "columns": "`c` UInt64 -- the count",
            "append": true,
            "empty": true,
            "comment": "cada hora, it's",
            "enabled": false,
            "query": "SELECT count() AS c FROM db.src;\n",
        })),
        &c,
        &ch,
        None,
    )
    .unwrap();
    assert!(!enabled);
    let p = refresh::plan_create(&w, enabled, &c).unwrap();
    assert_eq!(p.id, "`db`.`dbine_test_sched_mv 'x'`");
    assert_eq!(p.statements.len(), 2);
    assert_eq!(
        p.statements[0],
        "CREATE MATERIALIZED VIEW `db`.`dbine_test_sched_mv 'x'`\n\
         REFRESH EVERY 1 HOUR OFFSET 5 MINUTE RANDOMIZE FOR 1 MINUTE DEPENDS ON `db`.`a`, `x y`.`b` SETTINGS refresh_retries = 3, all_replicas = 1, refresh_future = 'on', other = -1.5 APPEND\n\
         (\n    `c` UInt64 -- the count\n)\n\
         ENGINE = MergeTree ORDER BY c\n\
         EMPTY\n\
         DEFINER = CURRENT_USER SQL SECURITY DEFINER\n\
         AS\nSELECT count() AS c FROM db.src\n\
         COMMENT 'cada hora, it\\'s'"
    );
    assert_eq!(
        p.statements[1],
        "SYSTEM STOP VIEW `db`.`dbine_test_sched_mv 'x'`"
    );
    let script = p.script();
    assert!(
        script.starts_with(
            "-- Crea la vista refrescable db.dbine_test_sched_mv 'x'.\n-- SYSTEM STOP VIEW"
        ),
        "{script}"
    );
    assert!(!script.contains("\nSET "), "{script}");
    // Its query's unqualified names are read in the view's database.
    assert_eq!(p.database.as_deref(), Some("db"));
    assert!(
        script.contains("\nUSE `db`;\nCREATE MATERIALIZED VIEW"),
        "{script}"
    );
    // The editor splits it into the same statements.
    assert_eq!(
        crate::statements(&script).len(),
        in_editor(&p),
        "{:#?}",
        crate::statements(&script)
    );

    // Before 24.10: the experimental setting, shown as a SET.
    let old = ctx("24.8.14.39");
    let (w, enabled) = refresh::wanted(&view_values(json!({})), &old, &ch, None).unwrap();
    let p = refresh::plan_create(&w, enabled, &old).unwrap();
    assert_eq!(
        p.settings,
        vec![("allow_experimental_refreshable_materialized_view", "1")]
    );
    assert!(p.script().contains(
        "\nSET allow_experimental_refreshable_materialized_view = 1;\nUSE `db`;\nCREATE MATERIALIZED VIEW"
    ));
    // A TO target: no engine.
    let (w, _) = refresh::wanted(
        &view_values(json!({"target_mode": "to", "target": "`db`.`dst`"})),
        &c,
        &ch,
        None,
    )
    .unwrap();
    let sql = refresh::create_sql(&w, false, true);
    assert!(
        sql.contains("\nTO `db`.`dst`\n") && !sql.contains("ENGINE"),
        "{sql}"
    );
}

fn current() -> ViewDef {
    let mut d = parse_view(FULL).unwrap();
    d.database = "db".into();
    d.name = "mv".into();
    d
}

fn edit(changes: Value) -> (ViewDef, bool) {
    let cur = current();
    let ch = choices();
    let form = refresh::build_form(&cur, true, true, &new_ch(), &ch);
    let mut values = form.values.clone();
    for (k, x) in changes.as_object().unwrap() {
        values.insert(k.clone(), x.clone());
    }
    refresh::wanted(
        &FormValues {
            values,
            lists: BTreeMap::new(),
        },
        &new_ch(),
        &ch,
        Some(&cur),
    )
    .unwrap()
}

fn edit_plan(changes: Value) -> Plan {
    let (w, enabled) = edit(changes);
    refresh::plan_edit(&current(), true, &w, enabled, &new_ch(), true).unwrap()
}

#[test]
fn each_change_is_its_own_alter() {
    let p = edit_plan(json!({"query": "SELECT 2 AS c"}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY QUERY\nSELECT 2 AS c"]
    );
    assert_eq!(p.database.as_deref(), Some("db"));
    assert!(edit_plan(json!({"comment": "x"})).database.is_none());

    let p = edit_plan(json!({"mode": "after", "interval": "2 HOUR"}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY REFRESH AFTER 2 HOUR RANDOMIZE FOR 1 HOUR 30 MINUTE SETTINGS refresh_retries = 3, refresh_retry_initial_backoff_ms = 200, refresh_retry_max_backoff_ms = 5000"]
    );
    let p = edit_plan(json!({"refresh_retries": null, "depends_on": ["`db`.`a`"]}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY REFRESH EVERY 1 MONTH OFFSET 5 DAY 2 HOUR RANDOMIZE FOR 1 HOUR 30 MINUTE DEPENDS ON `db`.`a` SETTINGS refresh_retry_initial_backoff_ms = 200, refresh_retry_max_backoff_ms = 5000"]
    );

    let p = edit_plan(json!({"sql_security": "none"}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY SQL SECURITY NONE"]
    );
    let p = edit_plan(json!({"definer": "ana"}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY SQL SECURITY DEFINER DEFINER = `ana`"]
    );

    let p = edit_plan(json!({"comment": "it's"}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY COMMENT 'it\\'s'"]
    );
    let p = edit_plan(json!({"comment": ""}));
    assert_eq!(p.statements, ["ALTER TABLE `db`.`mv` MODIFY COMMENT ''"]);

    let p = edit_plan(json!({"enabled": false}));
    assert_eq!(p.statements, ["SYSTEM STOP VIEW `db`.`mv`"]);
    assert!(p.comments.iter().any(|c| c.contains("se reinicia")));
    let (w, _) = edit(json!({}));
    let p = refresh::plan_edit(&current(), false, &w, true, &new_ch(), true).unwrap();
    assert_eq!(p.statements, ["SYSTEM START VIEW `db`.`mv`"]);

    // Renamed, with a new query: the ALTERs on the old name, then the
    // rename, then the state on the new name.
    let p = edit_plan(json!({"name": "mv2", "query": "SELECT 3 AS c", "enabled": false}));
    assert_eq!(
        p.statements,
        [
            "ALTER TABLE `db`.`mv` MODIFY QUERY\nSELECT 3 AS c",
            "RENAME TABLE `db`.`mv` TO `db`.`mv2`",
            "SYSTEM STOP VIEW `db`.`mv2`",
        ]
    );
    assert_eq!(p.id, "`db`.`mv2`");
    // Into another database, with its own table: created there, the old
    // one dropped.
    let p = edit_plan(
        json!({"database": "x y", "name": "mv2", "query": "SELECT 3 AS c", "enabled": false}),
    );
    assert_eq!(p.statements.len(), 3, "{p:?}");
    assert!(
        p.statements[0].starts_with("CREATE MATERIALIZED VIEW `x y`.`mv2`\nREFRESH EVERY 1 MONTH")
    );
    assert!(
        p.statements[0].ends_with("AS\nSELECT 3 AS c\nCOMMENT 'hola'"),
        "{}",
        p.statements[0]
    );
    assert_eq!(p.statements[1], "SYSTEM STOP VIEW `x y`.`mv2`");
    assert_eq!(p.statements[2], "DROP VIEW `db`.`mv`");
    assert!(p
        .comments
        .iter()
        .any(|c| c.contains("no mueve a otra base de datos")));
    assert_eq!(p.id, "`x y`.`mv2`");
    // A view writing to a table moves with RENAME.
    let p = edit_plan(json!({"database": "x y", "target_mode": "to", "target": "`db`.`dst`"}));
    assert!(
        p.statements[0].starts_with("CREATE OR REPLACE MATERIALIZED VIEW `db`.`mv`\n"),
        "{p:?}"
    );
    assert_eq!(p.statements[1], "RENAME TABLE `db`.`mv` TO `x y`.`mv`");
    // Only spaces changed in the query: nothing to do.
    let p = edit_plan(json!({"query": "SELECT count()   AS c FROM dbine_test_sched_x.src"}));
    assert!(p.statements.is_empty(), "{p:?}");
}

#[test]
fn an_append_view_keeps_append_in_its_refresh() {
    let mut cur = parse_view(APPEND_TO).unwrap();
    cur.database = "db".into();
    cur.name = "b".into();
    cur.to = Some(("db".into(), "dst".into()));
    cur.depends_on = vec![("db".into(), "a".into())];
    let ch = choices();
    let form = refresh::build_form(&cur, true, true, &new_ch(), &ch);
    let mut values = form.values.clone();
    values.insert("interval".into(), json!("1 MINUTE"));
    let (w, enabled) = refresh::wanted(
        &FormValues {
            values,
            lists: BTreeMap::new(),
        },
        &new_ch(),
        &ch,
        Some(&cur),
    )
    .unwrap();
    let p = refresh::plan_edit(&cur, true, &w, enabled, &new_ch(), true).unwrap();
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`b` MODIFY REFRESH AFTER 1 MINUTE DEPENDS ON `db`.`a` APPEND"]
    );
}

#[test]
fn what_cant_change_in_place_creates_the_view_again() {
    let p = edit_plan(json!({"engine": "MergeTree ORDER BY tuple()", "enabled": false}));
    assert_eq!(p.statements.len(), 2);
    assert!(
        p.statements[0]
            .starts_with("CREATE OR REPLACE MATERIALIZED VIEW `db`.`mv`\nREFRESH EVERY 1 MONTH"),
        "{p:?}"
    );
    assert!(p.statements[0].contains("\nENGINE = MergeTree ORDER BY tuple()\n"));
    assert_eq!(p.statements[1], "SYSTEM STOP VIEW `db`.`mv`");
    let script = p.script();
    assert!(
        script.contains("-- ClickHouse no cambia en su lugar"),
        "{script}"
    );
    assert!(
        script.contains("tabla propia se vuelve a crear vacía"),
        "{script}"
    );

    // Created again, it starts: a stopped view the user wants started
    // needs nothing more.
    let (w, _) = edit(json!({"append": true}));
    let p = refresh::plan_edit(&current(), false, &w, true, &new_ch(), true).unwrap();
    assert_eq!(p.statements.len(), 1);
    assert!(p.statements[0].contains(" APPEND\n"), "{p:?}");

    // To a target table: its rows stay.
    let (w, _) = edit(json!({"target_mode": "to", "target": "`db`.`dst`"}));
    let p = refresh::plan_edit(&current(), true, &w, true, &new_ch(), true).unwrap();
    assert!(p.statements[0].contains("\nTO `db`.`dst`\n") && !p.statements[0].contains("ENGINE"));
    assert!(p.comments.iter().any(|c| c.contains("conserva sus filas")));

    // A database without REPLACE: drop and create, and the old view made
    // again when the server refuses the new one.
    let (w, _) = edit(json!({"columns": "`c` UInt64, `d` UInt8"}));
    let p = refresh::plan_edit(&current(), true, &w, true, &new_ch(), false).unwrap();
    assert_eq!(p.statements[0], "DROP VIEW `db`.`mv`");
    assert!(p.statements[1].starts_with("CREATE MATERIALIZED VIEW `db`.`mv`\n"));
    assert!(choices().replaceable("db") && !choices().replaceable("old"));
    let r = p.restore.as_ref().expect("a drop and create restores");
    assert_eq!((r.drop, r.create), (0, 1));
    assert_eq!(r.what, "db.mv");
    assert_eq!(r.statements, [refresh::create_sql(&current(), false, true)]);
    assert!(
        r.statements[0].contains("\n(\n    `c` UInt64\n)\n") && !r.statements[0].contains("EMPTY")
    );
    assert!(p.script().contains(
        "-- Si el servidor rechaza la vista nueva, DBine vuelve a crear db.mv como estaba.\n"
    ));
    assert!(r
        .message("Code: 36. BAD_ARGUMENTS", Ok(()))
        .contains("se volvió a crear «db.mv» como estaba"));
    let m = r.message("x", Err("y".into()));
    assert!(
        m.contains("tampoco se pudo volver a crear «db.mv» como estaba (y)"),
        "{m}"
    );
    assert!(m.ends_with(&r.statements[0]), "{m}");
    // A stopped view is stopped again.
    let p = refresh::plan_edit(&current(), false, &w, false, &new_ch(), false).unwrap();
    assert_eq!(
        p.restore.unwrap().statements[1],
        "SYSTEM STOP VIEW `db`.`mv`"
    );
    // With REPLACE, or changed in place, there is nothing to restore.
    let p = refresh::plan_edit(&current(), true, &w, true, &new_ch(), true).unwrap();
    assert!(p.restore.is_none());
    assert!(edit_plan(json!({"query": "SELECT 2 AS c"}))
        .restore
        .is_none());
}

#[test]
fn the_refresh_clause_changes_only_when_its_meaning_does() {
    // Created outside DBine: its own settings first, a boolean spelled
    // `true`, two DEPENDS ON.
    let mut cur = parse_view(
        "CREATE MATERIALIZED VIEW db.mv REFRESH EVERY 1 HOUR DEPENDS ON `x y`.b, db.a SETTINGS foo = 1, refresh_retries = 2, all_replicas = true (`c` UInt64) ENGINE = Memory AS SELECT 1 AS c",
    )
    .unwrap();
    cur.depends_on = vec![("x y".into(), "b".into()), ("db".into(), "a".into())];
    let ch = choices();
    let form = refresh::build_form(&cur, true, true, &new_ch(), &ch);
    let read = |changes: Value| {
        let mut values = form.values.clone();
        for (k, x) in changes.as_object().unwrap() {
            values.insert(k.clone(), x.clone());
        }
        let (w, enabled) = refresh::wanted(
            &FormValues {
                values,
                lists: BTreeMap::new(),
            },
            &new_ch(),
            &ch,
            Some(&cur),
        )
        .unwrap();
        refresh::plan_edit(&cur, true, &w, enabled, &new_ch(), true).unwrap()
    };
    // Read back as it is (the DEPENDS ON picked in another order): nothing.
    let p = read(json!({"depends_on": ["`db`.`a`", "`x y`.`b`"]}));
    assert!(p.statements.is_empty(), "{p:?}");
    // Only the comment: no MODIFY REFRESH.
    let p = read(json!({"comment": "nuevo"}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY COMMENT 'nuevo'"]
    );
    // A real change keeps the server's order and spelling.
    let p = read(json!({"refresh_retries": 3}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY REFRESH EVERY 1 HOUR DEPENDS ON `x y`.`b`, `db`.`a` SETTINGS foo = 1, refresh_retries = 3, all_replicas = true"]
    );
    let p = read(json!({"all_replicas": false}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY REFRESH EVERY 1 HOUR DEPENDS ON `x y`.`b`, `db`.`a` SETTINGS foo = 1, refresh_retries = 2"]
    );
    let p = read(json!({"depends_on": ["`x y`.`b`"], "other_settings": "foo = 2, bar = 'x'"}));
    assert_eq!(
        p.statements,
        ["ALTER TABLE `db`.`mv` MODIFY REFRESH EVERY 1 HOUR DEPENDS ON `x y`.`b` SETTINGS foo = 2, refresh_retries = 2, all_replicas = true, bar = 'x'"]
    );
    // `false` and `0` mean the same; an OFFSET without EVERY counts for
    // nothing.
    let mut a = cur.clone();
    a.settings = vec![("all_replicas".into(), "false".into())];
    let mut b = a.clone();
    b.settings = vec![("all_replicas".into(), "0".into())];
    assert!(refresh::same_refresh(&a, &b));
    a.every = false;
    b.every = false;
    b.offset = Some(vec![(5, Unit::Minute)]);
    assert!(refresh::same_refresh(&a, &b));
    b.every = true;
    assert!(!refresh::same_refresh(&a, &b));
}

#[test]
fn deleting_and_clearing() {
    let p = refresh::drop_plan(&current());
    assert!(p.database.is_none());
    assert_eq!(
        p.script(),
        "-- Borra la vista refrescable db.mv.\n-- Su tabla propia se borra con ella, con sus datos.\nDROP VIEW `db`.`mv`;\n"
    );
    let mut d = current();
    d.to = Some(("db".into(), "dst".into()));
    assert!(refresh::drop_plan(&d)
        .script()
        .contains("La tabla destino db.dst queda"));

    let sql = clear_history_sql("d'b", "v\\1");
    assert!(sql.starts_with(
        "DELETE FROM system.query_log\nWHERE client_name = 'refreshable materialized view'"
    ));
    assert!(
        sql.contains("log_comment = 'refresh of d\\'b.v\\\\1'"),
        "{sql}"
    );
    assert!(
        sql.contains("startsWith(log_comment, 'refresh of d\\'b.v\\\\1 (attempt ')"),
        "{sql}"
    );
    // The byte after "refresh of d'b.v\1 (attempt ".
    assert!(sql.contains("substring(log_comment, 29)"), "{sql}");
    one_statement(&sql).unwrap();
}

#[test]
fn the_log_says_how_each_refresh_went() {
    let row = |ty: &str, code: i64| {
        refresh_log_row(&[
            json!("2026-11-12 21:46:37"),
            json!("db.mv (attempt 2/3)"),
            json!(ty),
            json!(code),
            json!("12"),
            json!(10),
            json!(1),
            json!(""),
        ])
    };
    let ok = row("QueryFinish", 0);
    assert_eq!(ok[1], json!("db.mv (attempt 2/3)"));
    assert_eq!(ok[2], json!("Correcto"));
    assert_eq!(ok[3], json!(12));
    assert_eq!(row("ExceptionWhileProcessing", 394)[2], json!("Cancelado"));
    assert_eq!(row("ExceptionBeforeStart", 60)[2], json!("Error"));
}

#[test]
fn the_contract_refuses_foreign_kinds_and_ids() {
    assert!(check_kind("job").is_ok());
    for k in ["schedule", "", "job; DROP"] {
        assert!(err_of(check_kind(k)).contains("no es un tipo"), "{k}");
    }
    for id in ["db.mv", "`db`.`mv`; DROP TABLE x", "`db`", "x' OR 1=1 --"] {
        assert!(err_of(ids(id)).contains("no es un id"), "{id}");
    }
    assert_eq!(
        ids("`a b`.`c`").unwrap(),
        ("a b".to_string(), "c".to_string())
    );
    assert!(one_statement("SELECT 1; SELECT 2").is_err());
    assert!(one_statement("SELECT 1").is_ok());
}

// ---------------------------------------------------------------------------
// Timeplus Proton tasks
// ---------------------------------------------------------------------------

#[test]
fn proton_intervals() {
    for (input, out) in [
        ("1h", "1h"),
        ("500ms", "500ms"),
        ("90s", "90s"),
        ("5 SECOND", "5s"),
        ("INTERVAL 10 minutes", "10m"),
        ("1 d", "1d"),
    ] {
        assert_eq!(proton_interval(input).as_deref(), Ok(out), "{input}");
    }
    assert_eq!(proton_interval("0s"), Err(BadInterval::Zero));
    assert_eq!(proton_interval("1M"), Err(BadInterval::TooLong));
    assert_eq!(proton_interval("2w"), Err(BadInterval::TooLong));
    assert_eq!(proton_interval("1 YEAR"), Err(BadInterval::TooLong));
    for bad in ["", "h", "1h30m", "1.5h", "-1h", "1 fortnight"] {
        assert_eq!(proton_interval(bad), Err(BadInterval::NotOne), "{bad}");
    }
}

const TASK: &str = "CREATE TASK default.dbine_test_sched_t1\nSCHEDULE 1h\nTIMEOUT 30m\nINTO default.`out put`\nAS \n  SELECT count() AS n FROM numbers(3) SETTINGS max_threads=1";

fn task_choices() -> task::Choices {
    task::Choices {
        databases: vec!["default".into(), "x y".into()],
        targets: vec![
            ("default".into(), "out put".into()),
            ("default".into(), "o2".into()),
        ],
    }
}

#[test]
fn proton_tasks_are_read_and_written() {
    let t = parse_task(TASK).unwrap();
    assert_eq!(
        t,
        TaskSpec {
            database: "default".into(),
            name: "dbine_test_sched_t1".into(),
            schedule: Schedule::Every("1h".into()),
            timeout: Some("30m".into()),
            into: Some(("default".into(), "out put".into())),
            query: "SELECT count() AS n FROM numbers(3) SETTINGS max_threads=1".into(),
        }
    );
    let sql = task::create_sql(&t, false);
    assert_eq!(
        sql,
        "CREATE TASK `default`.`dbine_test_sched_t1`\nSCHEDULE 1h\nTIMEOUT 30m\nINTO `default`.`out put`\nAS\nSELECT count() AS n FROM numbers(3) SETTINGS max_threads=1"
    );
    assert_eq!(parse_task(&sql).unwrap(), t);
    let t = parse_task(
        "CREATE TASK default.t3\nSCHEDULE '0 * * * *'\nTIMEOUT 1h\nAS \n  SELECT 1 AS n",
    )
    .unwrap();
    assert_eq!(t.schedule, Schedule::Cron("0 * * * *".into()));
    assert_eq!(t.into, None);
    assert!(task::create_sql(&t, true)
        .starts_with("CREATE OR REPLACE TASK `default`.`t3`\nSCHEDULE '0 * * * *'\n"));
    assert!(parse_task("CREATE STREAM default.s (a int)").is_none());
}

fn task_values(changes: Value) -> FormValues {
    let mut v = json!({
        "database": "default",
        "name": "dbine_test_sched_t1",
        "enabled": true,
        "schedule_mode": "interval",
        "schedule": "1h",
        "cron": "",
        "timeout": "30m",
        "into": "`default`.`out put`",
        "query": "SELECT count() AS n FROM numbers(3) SETTINGS max_threads=1",
    });
    for (k, x) in changes.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v)
}

#[test]
fn proton_task_values_are_checked() {
    let ch = task_choices();
    let refuse = |changes: Value, needle: &str| {
        let m = err_of(task::wanted(&task_values(changes.clone()), &ch, None));
        assert!(m.contains(needle), "{changes}: {m}");
    };
    refuse(json!({"database": "nope"}), "no existe");
    refuse(json!({"name": ""}), "obligatorio");
    refuse(json!({"schedule": ""}), "«Intervalo» es obligatorio");
    refuse(
        json!({"schedule": "1h30m"}),
        "no es un intervalo de Timeplus Proton",
    );
    refuse(json!({"schedule": "2w"}), "hasta días");
    refuse(json!({"timeout": "0s"}), "mayor que cero");
    refuse(
        json!({"schedule_mode": "cron", "cron": ""}),
        "«Expresión cron» es obligatorio",
    );
    refuse(
        json!({"schedule_mode": "cron", "cron": "0 * * *\n*"}),
        "no es una expresión cron",
    );
    refuse(json!({"schedule_mode": "weekly"}), "no es un modo");
    refuse(json!({"into": "`default`.`nope`"}), "no es un stream");
    refuse(json!({"query": "SELECT 1; DROP TASK x"}), "«Consulta»");
    let (t, enabled) =
        task::wanted(&task_values(json!({"timeout": "", "into": ""})), &ch, None).unwrap();
    assert!(enabled && t.timeout.is_none() && t.into.is_none());
    let (t, _) = task::wanted(
        &task_values(json!({"schedule_mode": "cron", "cron": "*/5 * * * *"})),
        &ch,
        None,
    )
    .unwrap();
    assert_eq!(t.schedule, Schedule::Cron("*/5 * * * *".into()));
}

#[test]
fn proton_task_plans() {
    let ch = task_choices();
    // New, paused.
    let (w, enabled) = task::wanted(&task_values(json!({"enabled": false})), &ch, None).unwrap();
    let p = task::plan_create(&w, enabled).unwrap();
    assert_eq!(
        p.statements,
        [
            "CREATE TASK `default`.`dbine_test_sched_t1`\nSCHEDULE 1h\nTIMEOUT 30m\nINTO `default`.`out put`\nAS\nSELECT count() AS n FROM numbers(3) SETTINGS max_threads=1",
            "SYSTEM PAUSE TASK `default`.`dbine_test_sched_t1`",
        ]
    );
    assert_eq!(crate::statements(&p.script()).len(), in_editor(&p));

    let cur = parse_task(TASK).unwrap();
    let form = task::build_form(&cur, Some(false), true, &ch);
    assert_eq!(form.values["enabled"], json!(false));
    assert!(form
        .notes
        .iter()
        .any(|n| n.contains("CREATE OR REPLACE TASK")));
    let edit = |changes: Value| {
        let mut values = form.values.clone();
        for (k, x) in changes.as_object().unwrap() {
            values.insert(k.clone(), x.clone());
        }
        let (w, enabled) = task::wanted(
            &FormValues {
                values,
                lists: BTreeMap::new(),
            },
            &ch,
            Some(&cur),
        )
        .unwrap();
        task::plan_edit(&cur, Some(false), &w, enabled).unwrap()
    };
    // As it came: nothing.
    let p = edit(json!({}));
    assert!(p.statements.is_empty());
    assert_eq!(
        p.script(),
        "-- default.dbine_test_sched_t1: no hay cambios que guardar.\n"
    );
    // Unpaused only.
    assert_eq!(
        edit(json!({"enabled": true})).statements,
        ["SYSTEM UNPAUSE TASK `default`.`dbine_test_sched_t1`"]
    );
    // A new schedule: replaced in place, and paused again.
    let p = edit(json!({"schedule": "5 minutes"}));
    assert_eq!(p.statements.len(), 2);
    assert!(p.statements[0]
        .starts_with("CREATE OR REPLACE TASK `default`.`dbine_test_sched_t1`\nSCHEDULE 5m\n"));
    assert_eq!(
        p.statements[1],
        "SYSTEM PAUSE TASK `default`.`dbine_test_sched_t1`"
    );
    // Renamed: created, paused, the old one dropped.
    let p = edit(json!({"name": "t2", "database": "x y"}));
    assert_eq!(
        p.statements,
        [
            "CREATE TASK `x y`.`t2`\nSCHEDULE 1h\nTIMEOUT 30m\nINTO `default`.`out put`\nAS\nSELECT count() AS n FROM numbers(3) SETTINGS max_threads=1",
            "SYSTEM PAUSE TASK `x y`.`t2`",
            "DROP TASK `default`.`dbine_test_sched_t1`",
        ]
    );
    assert_eq!(p.id, "`x y`.`t2`");
    assert!(p.comments.iter().any(|c| c.contains("no renombra")));

    assert_eq!(
        task::drop_plan(&cur).script(),
        "-- Borra la tarea default.dbine_test_sched_t1 de Timeplus Proton.\nDROP TASK `default`.`dbine_test_sched_t1`;\n"
    );
}

#[test]
fn proton_task_form() {
    let f = task::build_form(&parse_task(TASK).unwrap(), None, true, &task_choices());
    let keys: Vec<&str> = f
        .sections
        .iter()
        .flat_map(|s| s.fields.iter().map(|x| x.key.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            "database",
            "name",
            "enabled",
            "schedule_mode",
            "schedule",
            "cron",
            "timeout",
            "into",
            "query"
        ]
    );
    let enabled = f.sections[0]
        .fields
        .iter()
        .find(|x| x.key == "enabled")
        .unwrap();
    assert!(enabled.read_only, "the state is unknown");
    assert!(f.notes.iter().any(|n| n.contains("readonly")));
    assert_eq!(f.values["into"], json!("`default`.`out put`"));
    assert_eq!(
        f.choices["stream"][0],
        (
            "`default`.`out put`".to_string(),
            "default.out put".to_string()
        )
    );
}

#[test]
fn hostile_names_stay_names_in_the_editor_too() {
    let odd = "x`; DROP DATABASE db; --";
    assert_eq!(qi(odd), "`x``; DROP DATABASE db; --`");
    assert_eq!(qi("a\\`b"), "`a\\\\``b`");
    assert_eq!(lex::unquote(&qi(odd)), odd);
    assert_eq!(lex::unquote(&qi("a\\`b")), "a\\`b");
    // The script DBine shows splits, in a query tab, into its own
    // statements only.
    let (w, enabled) = refresh::wanted(
        &view_values(json!({"name": odd, "enabled": false})),
        &new_ch(),
        &choices(),
        None,
    )
    .unwrap();
    let p = refresh::plan_create(&w, enabled, &new_ch()).unwrap();
    assert_eq!(
        crate::statements(&p.script()).len(),
        in_editor(&p),
        "{}",
        p.script()
    );
    assert_eq!(parse_view(&p.statements[0]).unwrap().name, odd);
    let p = refresh::drop_plan(&w);
    assert_eq!(
        crate::statements(&p.script()).len(),
        in_editor(&p),
        "{}",
        p.script()
    );
    let (t, enabled) = task::wanted(
        &task_values(json!({"name": odd, "enabled": false})),
        &task_choices(),
        None,
    )
    .unwrap();
    let p = task::plan_create(&t, enabled).unwrap();
    assert_eq!(
        crate::statements(&p.script()).len(),
        in_editor(&p),
        "{}",
        p.script()
    );
    assert_eq!(parse_task(&p.statements[0]).unwrap().name, odd);
}

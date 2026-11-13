//! Hive 4: scheduled queries.

use super::*;
use crate::scheduler::hive as e;

const COLS: &[&str] = &[
    "id",
    "name",
    "enabled",
    "namespace",
    "schedule",
    "owner",
    "query",
    "next_execution",
];

/// Query 5 `sc_analyze` in this session's namespace (hive), query 6
/// `remote` in another one.
fn server() -> Fake {
    Fake::default()
        .rows(
            "SET hive.scheduled.queries.namespace",
            &["set"],
            vec![vec![Some("hive.scheduled.queries.namespace=hive")]],
        )
        .rows(
            "q.scheduled_query_id = 5",
            COLS,
            vec![vec![
                Some("5"),
                Some("sc_analyze"),
                Some("true"),
                Some("hive"),
                Some("0 */10 * * * ? *"),
                Some("ana"),
                Some("analyze table t compute statistics"),
                Some("1762916400"),
            ]],
        )
        .rows(
            "q.scheduled_query_id = 6",
            COLS,
            vec![vec![
                Some("6"),
                Some("remote"),
                Some("false"),
                Some("other"),
                Some("0 0 * * * ? *"),
                Some("bob"),
                Some("select 1"),
                None,
            ]],
        )
}

fn new_query(over: Value) -> FormValues {
    let mut v = json!({
        "name": "nightly_load", "enabled": true, "executed_as": "etl",
        "mode": "cron", "cron": "0 0 3 * * ? *",
        "query": "insert into t select * from s where note = 'it''s';\n"
    });
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({}))
}

fn analyze(over: Value) -> FormValues {
    let f = form(preset("hive"), &mut server(), &ctx(), "job", Some("5")).unwrap();
    let mut v = serde_json::to_value(&f.values).unwrap();
    for (k, x) in over.as_object().unwrap() {
        v[k] = x.clone();
    }
    fv(v, json!({}))
}

#[test]
fn a_new_query_is_create_scheduled_query_with_every_clause() {
    let s = script(
        preset("hive"),
        &mut server(),
        "job",
        None,
        &new_query(json!({})),
    )
    .unwrap();
    assert_eq!(
        s,
        "-- Consulta programada «nightly_load» de Hive
CREATE SCHEDULED QUERY `nightly_load`
CRON '0 0 3 * * ? *'
EXECUTED AS 'etl'
ENABLED
DEFINED AS insert into t select * from s where note = 'it''s';
"
    );
}

#[test]
fn a_backslash_is_escaped_in_hive_string_literals() {
    // In Hive a backslash escapes inside a string: `a\'b` is written
    // `'a\\\'b'`, not `'a\'b'` (which would end the string after it).
    let q = e::from_values(&new_query(json!({"executed_as": "a\\'b"}))).unwrap();
    assert!(
        e::create_statement(&q).contains("\nEXECUTED AS 'a\\\\\\'b'\n"),
        "{}",
        e::create_statement(&q)
    );
    let v = analyze(json!({"executed_as": "x\\"}));
    let s = script(preset("hive"), &mut server(), "job", Some("5"), &v).unwrap();
    assert!(
        s.contains("ALTER SCHEDULED QUERY `sc_analyze` EXECUTED AS 'x\\\\';"),
        "{s}"
    );
}

#[test]
fn every_schedules_have_their_unit_and_offset() {
    let q = e::from_values(&new_query(json!({
        "mode": "every", "every": 2, "unit": "minute", "offset_mode": "offset", "offset": "00:00:30",
        "executed_as": null, "enabled": false
    })))
    .unwrap();
    assert_eq!(
        e::schedule_clause(&q),
        "EVERY 2 MINUTES OFFSET BY '00:00:30'"
    );
    assert_eq!(
        e::create_statement(&q),
        "CREATE SCHEDULED QUERY `nightly_load`\nEVERY 2 MINUTES OFFSET BY '00:00:30'\nDISABLED\nDEFINED AS insert into t select * from s where note = 'it''s'"
    );
    let q = e::from_values(&new_query(json!({
        "mode": "every", "every": 1, "unit": "DAY", "offset_mode": "at", "offset": "03:30:00"
    })))
    .unwrap();
    assert_eq!(e::schedule_clause(&q), "EVERY 1 DAY AT '03:30:00'");
}

#[test]
fn an_edit_is_one_alter_per_change() {
    let v = analyze(json!({
        "cron": "0 */15 * * * ? *", "enabled": false, "executed_as": "o'neil",
        "query": "analyze table t compute statistics for columns"
    }));
    assert_eq!(
        script(preset("hive"), &mut server(), "job", Some("5"), &v).unwrap(),
        "-- Consulta programada «sc_analyze» de Hive
ALTER SCHEDULED QUERY `sc_analyze` CRON '0 */15 * * * ? *';
ALTER SCHEDULED QUERY `sc_analyze` EXECUTED AS 'o\\'neil';
ALTER SCHEDULED QUERY `sc_analyze` DEFINED AS analyze table t compute statistics for columns;
ALTER SCHEDULED QUERY `sc_analyze` DISABLED;
"
    );
    assert_eq!(
        script(
            preset("hive"),
            &mut server(),
            "job",
            Some("5"),
            &analyze(json!({}))
        )
        .unwrap(),
        "-- «sc_analyze»: no hay cambios que guardar.\n"
    );
    let v = analyze(json!({"mode": "every", "every": 10, "unit": "MINUTE"}));
    assert!(script(preset("hive"), &mut server(), "job", Some("5"), &v)
        .unwrap()
        .contains("ALTER SCHEDULED QUERY `sc_analyze` EVERY 10 MINUTES;"));
}

#[test]
fn a_new_name_drops_and_creates_the_query() {
    let v = analyze(json!({"name": "sc_analyze2"}));
    let s = script(preset("hive"), &mut server(), "job", Some("5"), &v).unwrap();
    let drop = s.find("DROP SCHEDULED QUERY `sc_analyze`;").unwrap();
    let create = s.find("CREATE SCHEDULED QUERY `sc_analyze2`").unwrap();
    assert!(drop < create, "{s}");
    assert!(s.contains("-- Hive no cambia el nombre"), "{s}");
}

#[test]
fn a_query_of_another_namespace_is_only_read() {
    let f = form(preset("hive"), &mut server(), &ctx(), "job", Some("6")).unwrap();
    assert!(f.read_only);
    assert!(f.notes[0].contains("namespace «other»"));
    let v = fv(serde_json::to_value(&f.values).unwrap(), json!({}));
    assert!(matches!(
        script(preset("hive"), &mut server(), "job", Some("6"), &v),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        delete_script(preset("hive"), &mut server(), "job", "6"),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn bad_values_are_refused() {
    for (over, want) in [
        (json!({"name": "nightly-load"}), "letras, números y _"),
        (json!({"cron": "0 3 * * *"}), "6 o 7 campos"),
        (json!({"mode": "every", "every": 0}), "«Cada»"),
        (
            json!({"mode": "every", "every": 1, "unit": "WEEK"}),
            "«Unidad»",
        ),
        (
            json!({"mode": "every", "every": 1, "offset_mode": "at", "offset": "3'; DROP"}),
            "«Hora o desfase»",
        ),
        (json!({"query": " ; "}), "Falta «Consulta»."),
        (json!({"executed_as": "a\nb"}), "«Ejecuta como»"),
    ] {
        let m = refused(e::from_values(&new_query(over.clone())));
        assert!(m.contains(want), "{over}: {m}");
    }
}

#[test]
fn run_now_and_stop_are_the_kinds_actions() {
    let i = info(preset("hive"), &mut server(), &ctx()).unwrap();
    let k = &i.kinds[0];
    let keys: Vec<&str> = k.actions.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, ["run", "stop"]);
    assert_eq!(k.actions[0].label, "Ejecutar ahora");
    // The metastore ages executions out on its own; one step per query.
    assert!(!k.clear_history && !k.run_from_step);
    let s = action_script(preset("hive"), &mut server(), &ctx(), "job", "5", "run").unwrap();
    assert_eq!(
        s,
        "-- Pasa la próxima ejecución de la consulta programada a ahora: la corre el planificador de Hive.\nALTER SCHEDULED QUERY `sc_analyze` EXECUTE;\n"
    );
    let mut f = server();
    action(preset("hive"), &mut f, &ctx(), "job", "5", "run").unwrap();
    assert!(f.sent("ALTER SCHEDULED QUERY `sc_analyze` EXECUTE"));
    // Another namespace's query: ALTER can't reach it.
    assert!(action_script(preset("hive"), &mut server(), &ctx(), "job", "6", "run").is_err());
    // Nothing running: nothing to stop.
    let m = action_script(preset("hive"), &mut server(), &ctx(), "job", "5", "stop")
        .unwrap_err()
        .to_string();
    assert!(m.contains("no se está ejecutando"), "{m}");
    assert!(matches!(
        action_script(preset("hive"), &mut server(), &ctx(), "job", "5", "refresh"),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn delete_save_and_info() {
    assert_eq!(
        delete_script(preset("hive"), &mut server(), "job", "5").unwrap(),
        "-- Borra la consulta programada «sc_analyze» de Hive y sus ejecuciones\nDROP SCHEDULED QUERY `sc_analyze`;\n"
    );
    let mut f = server().rows(
        "WHERE q.schedule_name = 'nightly_load'",
        &["id", "namespace"],
        vec![
            vec![Some("9"), Some("other")],
            vec![Some("12"), Some("hive")],
        ],
    );
    let id = save(preset("hive"), &mut f, "job", None, &new_query(json!({}))).unwrap();
    assert_eq!(id.as_deref(), Some("12"));
    assert!(f.sent("CREATE SCHEDULED QUERY `nightly_load`"));
    assert!(!f.sent("insert into t select * from s where note = 'it''s';"));
    let i = info(preset("cloudera"), &mut server(), &ctx()).unwrap();
    assert_eq!(i.kinds[0].key, "job");
    assert!(i.notes[0].contains("«hive»"));
    let mut f = Fake::default().fail("information_schema.scheduled_queries", Some("10001"));
    let i = info(preset("hive"), &mut f, &ctx()).unwrap();
    assert!(i.kinds.is_empty() && i.notes[0].contains("Hive 4.0"));
    let f = form(preset("hive"), &mut server(), &ctx(), "job", Some("5")).unwrap();
    well_formed(&f);
    assert_eq!(val(&f, "mode"), "cron");
    assert_eq!(val(&f, "namespace"), "hive");
    assert!(!f.read_only);
    well_formed(&form(preset("hive"), &mut server(), &ctx(), "job", None).unwrap());
}

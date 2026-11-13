//! Unit tests of the OSchedule designer: rules, values, scripts, access,
//! the form (no server).

use super::*;
use serde_json::json;

fn map(v: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

fn fv(values: Value, args: Value) -> FormValues {
    FormValues {
        values: map(values),
        lists: BTreeMap::from([(
            "arguments".to_string(),
            serde_json::from_value(args).unwrap(),
        )]),
    }
}

fn funcs() -> Vec<Func> {
    vec![
        Func {
            name: "purge".into(),
            language: Some("SQL".into()),
            params: vec!["days".into(), "tag".into()],
        },
        Func {
            name: "free".into(),
            language: Some("javascript".into()),
            params: vec![],
        },
    ]
}

fn event(name: &str) -> Event {
    Event {
        rid: Some("#9:0".into()),
        name: name.into(),
        rule: Some("0 0 3 * * ?".into()),
        status: Some("STOPPED".into()),
        start_ms: Some(1_794_499_904_004),
        runs: Some(4),
        arguments: Some(json!({"days": 7, "tag": "x"})),
        function: Some("purge".into()),
        language: Some("SQL".into()),
        code: Some("DELETE FROM Log".into()),
        params: Some(json!(["days", "tag"])),
        idempotent: None,
    }
}

fn base() -> Value {
    json!({"name": "nightly", "function": "purge", "rule": "0 0 3 * * ?"})
}

#[test]
fn only_the_job_kind_exists() {
    assert!(check_kind("job").is_ok());
    assert!(check_kind(" job ").is_ok());
    for k in ["schedule", "", "job'; DROP"] {
        assert!(check_kind(k).is_err(), "{k}");
    }
}

#[test]
fn quartz_rules_are_checked_before_the_server() {
    for ok in [
        "0 0/5 * * * ?",
        "*/10 * * * * ?",
        "0 0 3 ? * MON-FRI",
        "0 0 3 ? * 2,4,6",
        "0 15 10 L * ?",
        "0 15 10 L-3 * ?",
        "0 15 10 15W * ?",
        "0 15 10 LW * ?",
        "0 0 12 ? * 6#3 2027",
        "0 0 12 ? * 6L",
        "0 0 12 1 JAN-MAR ?",
        "0 0 12 1 1/2 ? 2026-2030",
        "0  0\t3 * * ?",
    ] {
        assert!(check_rule(ok).is_ok(), "{ok}: {:?}", check_rule(ok));
    }
    for bad in [
        "",
        "x",
        "0 0 3 * *",
        "0 0 3 * * * * *",
        "0 0 3 * * *",
        "0 0 3 ? * ?",
        "60 * * * * ?",
        "0 60 * * * ?",
        "0 0 24 * * ?",
        "0 0 3 32 * ?",
        "0 0 3 * 13 ?",
        "0 0 3 ? * 8",
        "0 0 3 ? * 6#6",
        "0 0 3 * * ? 1969",
        "0/0 * * * * ?",
        "0 0 3 ,1 * ?",
        "0 0 3 * * ?\nDELETE FROM OSchedule",
        "? 0 3 * * ?",
        "0 0 3 * FOO ?",
    ] {
        assert!(check_rule(bad).is_err(), "{bad:?}");
    }
    assert!(check_rule("0 0 3 * *").unwrap_err().contains("tiene 5"));
}

#[test]
fn values_are_checked_and_typed() {
    let w = wanted(
        &fv(
            base(),
            json!([
                {"name": "days", "type": "number", "value": " 7 "},
                {"name": "tag", "type": "text", "value": " a b "}
            ]),
        ),
        &funcs(),
    )
    .unwrap();
    assert_eq!(w.name, "nightly");
    assert_eq!(w.arguments["days"], json!(7));
    // Text keeps its spaces.
    assert_eq!(w.arguments["tag"], json!(" a b "));

    // A function without declared parameters takes any name; JSON and
    // booleans are typed.
    let w = wanted(
        &fv(
            json!({"name": "n", "function": "free", "rule": "0  0 3 * * ?"}),
            json!([
                {"name": "any", "type": "json", "value": "{\"a\": [1, \"x\"]}"},
                {"name": "on", "type": "boolean", "value": "true"},
                {"name": "f", "type": "number", "value": "2.5"}
            ]),
        ),
        &funcs(),
    )
    .unwrap();
    assert_eq!(w.rule, "0 0 3 * * ?");
    assert_eq!(w.arguments["any"], json!({"a": [1, "x"]}));
    assert_eq!(w.arguments["on"], json!(true));
    assert_eq!(w.arguments["f"], json!(2.5));

    let err =
        |values: Value, args: Value| wanted(&fv(values, args), &funcs()).unwrap_err().to_string();
    assert!(err(
        json!({"function": "purge", "rule": "0 0 3 * * ?"}),
        json!([])
    )
    .contains("nombre"));
    assert!(err(
        json!({"name": "a\nb", "function": "purge", "rule": "0 0 3 * * ?"}),
        json!([])
    )
    .contains("saltos"));
    assert!(err(
        json!({"name": "x".repeat(201), "function": "purge", "rule": "0 0 3 * * ?"}),
        json!([])
    )
    .contains("200"));
    assert!(err(json!({"name": "a", "rule": "0 0 3 * * ?"}), json!([])).contains("función"));
    assert!(err(
        json!({"name": "a", "function": "nope", "rule": "0 0 3 * * ?"}),
        json!([])
    )
    .contains("No existe la función"));
    assert!(err(json!({"name": "a", "function": "purge"}), json!([])).contains("regla"));
    assert!(err(
        json!({"name": "a", "function": "purge", "rule": "bad"}),
        json!([])
    )
    .contains("6 o 7"));
    assert!(err(base(), json!([{"name": "other", "value": "1"}])).contains("no tiene el parámetro"));
    assert!(err(base(), json!([{"name": "", "value": "1"}])).contains("parámetro"));
    assert!(err(
        base(),
        json!([{"name": "days", "value": "1"}, {"name": "days", "value": "2"}])
    )
    .contains("dos veces"));
    assert!(err(
        base(),
        json!([{"name": "days", "type": "number", "value": "x"}])
    )
    .contains("número"));
    assert!(err(
        base(),
        json!([{"name": "days", "type": "boolean", "value": "x"}])
    )
    .contains("true o false"));
    assert!(err(
        base(),
        json!([{"name": "days", "type": "json", "value": "{"}])
    )
    .contains("JSON"));
    assert!(err(
        base(),
        json!([{"name": "days", "type": "sql", "value": "1"}])
    )
    .contains("tipo"));
}

#[test]
fn a_new_event_is_one_insert_with_everything_quoted() {
    let w = wanted(
        &fv(
            json!({"name": "it's \\ \"q\" */ x", "function": "purge", "rule": "0 0 3 * * ?"}),
            json!([
                {"name": "tag", "value": "a'; DELETE FROM OUser; --"},
                {"name": "days", "type": "number", "value": "7"}
            ]),
        ),
        &funcs(),
    )
    .unwrap();
    let p = plan(None, &w, &[event("other")], "db1").unwrap();
    assert_eq!(p.id, "it's \\ \"q\" */ x");
    assert_eq!(p.statements.len(), 1);
    assert_eq!(
        p.statements[0],
        "INSERT INTO OSchedule SET name = 'it\\'s \\\\ \"q\" */ x', rule = '0 0 3 * * ?', function = (SELECT FROM OFunction WHERE name = 'purge'), arguments = {'days': 7, 'tag': 'a\\'; DELETE FROM OUser; --'}"
    );
    // The comment can't be closed by the name; the script is the comment
    // and the statements.
    assert!(
        p.script
            .starts_with("/* Evento programado «it's \\ \"q\" * / x» de la base «db1» */\n"),
        "{}",
        p.script
    );
    assert!(p.script.ends_with(&format!("{};\n", p.statements[0])));
    assert_eq!(p.script.lines().next().unwrap().matches("*/").count(), 1);

    // No arguments: no map.
    let w = wanted(&fv(base(), json!([])), &funcs()).unwrap();
    let p = plan(None, &w, &[], "db1").unwrap();
    assert!(!p.statements[0].contains("arguments"));

    // A name another event has is refused.
    let e = plan(None, &w, &[event("nightly")], "db1").unwrap_err();
    assert!(e.to_string().contains("Ya hay"));
}

#[test]
fn an_edit_updates_only_what_changed() {
    let cur = event("nightly");
    let same = fv(
        base(),
        json!([
            {"name": "days", "type": "number", "value": "7.0"},
            {"name": "tag", "value": "x"}
        ]),
    );
    let w = wanted(&same, &funcs()).unwrap();
    let p = plan(Some(&cur), &w, std::slice::from_ref(&cur), "db1").unwrap();
    assert!(p.statements.is_empty());
    assert!(p.script.contains("no hay cambios"));
    assert_eq!(p.id, "nightly");

    // The rule alone.
    let mut v = base();
    v["rule"] = json!("0 0/10 * * * ?");
    let w = wanted(
        &fv(v, json!([{"name": "days", "type": "number", "value": "7"}, {"name": "tag", "value": "x"}])),
        &funcs(),
    )
    .unwrap();
    let p = plan(Some(&cur), &w, std::slice::from_ref(&cur), "db1").unwrap();
    assert_eq!(
        p.statements,
        ["UPDATE OSchedule SET rule = '0 0/10 * * * ?' WHERE name = 'nightly'"]
    );
    assert!(p.script.contains("cambia en el lugar"));

    // Function and arguments (removing them all writes an empty map).
    let mut v = base();
    v["function"] = json!("free");
    let w = wanted(&fv(v, json!([])), &funcs()).unwrap();
    let p = plan(Some(&cur), &w, std::slice::from_ref(&cur), "db1").unwrap();
    assert_eq!(
        p.statements,
        ["UPDATE OSchedule SET function = (SELECT FROM OFunction WHERE name = 'free'), arguments = {} WHERE name = 'nightly'"]
    );

    // An argument's value.
    let w = wanted(
        &fv(base(), json!([{"name": "days", "type": "number", "value": "8"}, {"name": "tag", "value": "x"}])),
        &funcs(),
    )
    .unwrap();
    let p = plan(Some(&cur), &w, std::slice::from_ref(&cur), "db1").unwrap();
    assert_eq!(
        p.statements,
        ["UPDATE OSchedule SET arguments = {'days': 8, 'tag': 'x'} WHERE name = 'nightly'"]
    );
}

#[test]
fn a_new_name_inserts_then_deletes_the_old_record() {
    let cur = event("nightly");
    let mut v = base();
    v["name"] = json!("nightly2");
    let w = wanted(&fv(v, json!([])), &funcs()).unwrap();
    let p = plan(Some(&cur), &w, std::slice::from_ref(&cur), "db1").unwrap();
    assert_eq!(p.id, "nightly2");
    assert_eq!(p.statements.len(), 2);
    assert!(p.statements[0].starts_with("INSERT INTO OSchedule SET name = 'nightly2'"));
    assert_eq!(
        p.statements[1],
        "DELETE FROM OSchedule WHERE name = 'nightly'"
    );
    assert!(p.script.contains("no permite cambiar el nombre"));
    // Its run counter starts again, and the two statements run apart.
    assert_eq!(warned(&p.script), [RENAME_NOTE, NOT_ATOMIC]);
    // Never an UPDATE of the name.
    assert!(!p.script.contains("UPDATE"));
    // The new name is taken.
    let e = plan(Some(&cur), &w, &[cur.clone(), event("nightly2")], "db1").unwrap_err();
    assert!(e.to_string().contains("Ya hay"));
}

#[test]
fn delete_is_one_statement() {
    let p = delete_plan(&event("it's"), "db1").unwrap();
    assert_eq!(
        p.statements,
        ["DELETE FROM OSchedule WHERE name = 'it\\'s'"]
    );
    assert!(p
        .script
        .starts_with("/* Borra el evento programado «it's» de la base «db1» */\n"));
}

#[test]
fn literals_quote_strings_and_keys() {
    assert_eq!(
        literal(&json!({"a'b": [1, true, null, "x\\y"], "n": {"m": 2.5}})),
        "{'a\\'b': [1, true, null, 'x\\\\y'], 'n': {'m': 2.5}}"
    );
}

#[test]
fn access_follows_the_roles_rules() {
    // The rules OrientDB 3.2 gives its built-in roles.
    let rows = vec![
        vec![
            ("name".to_string(), json!("admin")),
            (
                "rules".to_string(),
                json!({"database": 31, "database.class": 31, "*": 31}),
            ),
        ],
        vec![
            ("name".to_string(), json!("reader")),
            (
                "rules".to_string(),
                json!({"database": 2, "database.class": 2, "database.class.ouser": 0}),
            ),
        ],
        vec![
            ("name".to_string(), json!("writer")),
            (
                "rules".to_string(),
                json!({"database.class.oschedule": 2, "database.class": 31}),
            ),
        ],
        vec![
            ("name".to_string(), json!("ops")),
            ("rules".to_string(), json!({"database.class.oschedule": 15})),
            ("parent".to_string(), json!("reader")),
        ],
        vec![
            ("name".to_string(), json!("child")),
            ("rules".to_string(), json!({"database.function": 2})),
            ("parent".to_string(), json!("ADMIN")),
        ],
        vec![
            ("name".to_string(), json!("loop")),
            ("rules".to_string(), json!({})),
            ("parent".to_string(), json!("loop")),
        ],
    ];
    let roles = roles_of(&rows);
    let a = |r: &[&str]| access_of(&roles, &r.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(a(&["admin"]), Access::ALL);
    let none = Access {
        create: false,
        update: false,
        delete: false,
        known: true,
    };
    assert_eq!(a(&["reader"]), none);
    // writer may write classes, but OSchedule only read.
    assert_eq!(a(&["writer"]), none);
    assert_eq!(a(&["ops"]), Access::ALL);
    assert_eq!(a(&["child"]), Access::ALL);
    assert_eq!(a(&["reader", "admin"]), Access::ALL);
    assert_eq!(a(&["loop"]), none);
    assert_eq!(a(&["nobody"]), none);
    assert_eq!(a(&[]), none);
}

#[test]
fn functions_are_read_with_their_parameters() {
    let f = functions_of(&[
        vec![
            ("name".to_string(), json!("purge")),
            ("language".to_string(), json!("SQL")),
            ("parameters".to_string(), json!(["days", "tag"])),
        ],
        vec![("name".to_string(), json!(""))],
        vec![("name".to_string(), json!("bare"))],
    ]);
    assert_eq!(f.len(), 2);
    assert_eq!(function_label(&f[0]), "purge (SQL: days, tag)");
    assert_eq!(function_label(&f[1]), "bare");
}

#[test]
fn the_form_reads_the_event() {
    let f = form_of(Some(&event("nightly")), &funcs(), Access::ALL, "db1");
    assert!(!f.read_only);
    assert_eq!(f.values["name"], json!("nightly"));
    assert_eq!(f.values["function"], json!("purge"));
    assert_eq!(f.values["rule"], json!("0 0 3 * * ?"));
    assert_eq!(f.values["database"], json!("db1"));
    assert_eq!(f.values["runs"], json!(4));
    assert_eq!(f.values["last_start"], json!("2026-11-12T16:11:44Z"));
    let args = &f.sections[1].lists[0];
    assert_eq!(args.key, "arguments");
    assert_eq!(args.rows.len(), 2);
    assert_eq!(args.rows[0]["name"], json!("days"));
    assert_eq!(args.rows[0]["type"], json!("number"));
    assert_eq!(args.rows[0]["value"], json!("7"));
    assert_eq!(args.rows[1]["type"], json!("text"));
    assert_eq!(
        f.choices["function"][0],
        ("purge".to_string(), "purge (SQL: days, tag)".to_string())
    );
    // State fields are read-only.
    for k in ["status", "last_start", "runs", "database"] {
        let field = f.sections[0].fields.iter().find(|x| x.key == k).unwrap();
        assert!(field.read_only, "{k}");
    }
    assert!(f
        .notes
        .iter()
        .any(|n| n.contains("contador de ejecuciones")));

    // The values round-trip: saving the form as it came changes nothing.
    let values = FormValues {
        values: f.values.clone(),
        lists: BTreeMap::from([("arguments".to_string(), args.rows.clone())]),
    };
    let w = wanted(&values, &funcs()).unwrap();
    let cur = event("nightly");
    assert!(plan(Some(&cur), &w, std::slice::from_ref(&cur), "db1")
        .unwrap()
        .statements
        .is_empty());

    // A new one: defaults, no state fields; a function the list lacks
    // still shows for an existing event.
    let f = form_of(None, &funcs(), Access::ALL, "db1");
    assert_eq!(f.values["name"], json!(""));
    assert!(!f.sections[0].fields.iter().any(|x| x.key == "runs"));
    let mut e = event("x");
    e.function = Some("gone".into());
    let f = form_of(Some(&e), &funcs(), Access::ALL, "db1");
    assert!(f.choices["function"].iter().any(|(v, _)| v == "gone"));

    // Without the right to change it: read-only, with the reason first.
    let ro = Access {
        create: false,
        update: false,
        delete: false,
        known: true,
    };
    let f = form_of(Some(&event("nightly")), &funcs(), ro, "db1");
    assert!(f.read_only);
    assert_eq!(f.notes[0], NO_WRITE);
    let unknown = Access {
        known: false,
        ..Access::ALL
    };
    let f = form_of(None, &funcs(), unknown, "db1");
    assert!(!f.read_only && f.notes.iter().any(|n| n == UNKNOWN_ACCESS));
}

/// The script's warnings, as the app reads them.
fn warned(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|l| l.strip_prefix("/* ⚠ "))
        .filter_map(|l| l.strip_suffix(" */"))
        .map(str::to_string)
        .collect()
}

#[test]
fn only_a_new_name_warns() {
    let cur = event("nightly");
    let w = wanted(&fv(base(), json!([])), &funcs()).unwrap();
    // A new event, a change in place and a delete lose nothing.
    assert!(warned(&plan(None, &w, &[], "db1").unwrap().script).is_empty());
    let mut v = base();
    v["rule"] = json!("0 0/10 * * * ?");
    let w2 = wanted(&fv(v, json!([])), &funcs()).unwrap();
    let p = plan(Some(&cur), &w2, std::slice::from_ref(&cur), "db1").unwrap();
    assert!(warned(&p.script).is_empty(), "{}", p.script);
    assert!(warned(&delete_plan(&cur, "db1").unwrap().script).is_empty());
    // A new name: created again (its counter restarts), in two statements.
    let mut v = base();
    v["name"] = json!("nightly2");
    let w3 = wanted(&fv(v, json!([])), &funcs()).unwrap();
    let p = plan(Some(&cur), &w3, std::slice::from_ref(&cur), "db1").unwrap();
    assert_eq!(warned(&p.script), [RENAME_NOTE, NOT_ATOMIC]);
    assert!(p.script.starts_with(
        "/* Evento programado «nightly» de la base «db1», con el nombre nuevo «nightly2» */\n"
    ));
}

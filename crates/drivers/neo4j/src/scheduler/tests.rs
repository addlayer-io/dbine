//! Unit tests of the APOC jobs designer: values, calls, scripts, the form
//! (no server).

use super::*;
use serde_json::json;

fn fv(v: Value) -> FormValues {
    FormValues {
        values: serde_json::from_value(v).unwrap(),
        lists: BTreeMap::new(),
    }
}

fn job(name: &str, rate: i64, done: bool, cancelled: bool) -> Job {
    Job {
        name: name.into(),
        rate,
        delay: 0,
        done,
        cancelled,
    }
}

#[test]
fn only_the_job_kind_exists() {
    assert!(check_kind("job").is_ok());
    for k in ["schedule", "", "apoc"] {
        assert!(check_kind(k).is_err(), "{k}");
    }
}

#[test]
fn values_are_checked() {
    let w = wanted(&fv(json!({
        "name": " cleanup ",
        "type": "repeat",
        "rate": "30",
        "statement": "  MATCH (n:S) DELETE n  ",
        "params": "{\"limit\": 10}"
    })))
    .unwrap();
    assert_eq!(w.name, "cleanup");
    assert_eq!(w.mode, Mode::Repeat);
    assert_eq!(w.rate, 30);
    assert_eq!(w.statement, "MATCH (n:S) DELETE n");
    assert_eq!(w.params["limit"], json!(10));

    // Submit ignores the rate; countdown ignores the parameters.
    let w = wanted(&fv(json!({"name": "a", "type": "submit", "rate": "x", "statement": "RETURN 1", "params": {"a": 1}}))).unwrap();
    assert_eq!((w.mode, w.rate), (Mode::Submit, 0));
    assert_eq!(w.params["a"], json!(1));
    let w = wanted(&fv(json!({"name": "a", "type": "countdown", "rate": 5, "statement": "RETURN 0", "params": "nope"}))).unwrap();
    assert_eq!((w.mode, w.rate), (Mode::Countdown, 5));
    assert!(w.params.is_empty());
    // No type: a repeat; empty parameters: none.
    let w = wanted(&fv(
        json!({"name": "a", "rate": 1.0, "statement": "RETURN 1", "params": " "}),
    ))
    .unwrap();
    assert_eq!(w.mode, Mode::Repeat);
    assert!(w.params.is_empty());

    let err = |v: Value| wanted(&fv(v)).unwrap_err().to_string();
    assert!(err(json!({"statement": "RETURN 1", "rate": 1})).contains("nombre"));
    assert!(err(json!({"name": "a\nb", "statement": "RETURN 1", "rate": 1})).contains("saltos"));
    assert!(
        err(json!({"name": "x".repeat(513), "statement": "RETURN 1", "rate": 1})).contains("512")
    );
    assert!(err(json!({"name": "a", "rate": 1})).contains("consulta"));
    assert!(err(json!({"name": "a", "statement": "  ", "rate": 1})).contains("consulta"));
    assert!(err(json!({"name": "a", "statement": "RETURN '\u{0}'", "rate": 1})).contains("nulos"));
    assert!(err(json!({"name": "a", "type": "cron", "statement": "RETURN 1"})).contains("tipo"));
    assert!(err(json!({"name": "a", "statement": "RETURN 1"})).contains("segundos"));
    assert!(err(json!({"name": "a", "statement": "RETURN 1", "rate": 0})).contains("entre 1"));
    assert!(
        err(json!({"name": "a", "statement": "RETURN 1", "rate": 31_536_001})).contains("entre 1")
    );
    assert!(err(json!({"name": "a", "statement": "RETURN 1", "rate": "1.5"})).contains("entero"));
    assert!(
        err(json!({"name": "a", "statement": "RETURN 1", "rate": 1, "params": "[1]"}))
            .contains("objeto")
    );
    assert!(
        err(json!({"name": "a", "statement": "RETURN 1", "rate": 1, "params": "{"}))
            .contains("JSON")
    );
    assert!(
        err(json!({"name": "a", "statement": "RETURN 1", "rate": 1, "params": 3}))
            .contains("objeto")
    );
}

#[test]
fn calls_quote_the_name_the_statement_and_the_parameters() {
    let w = Wanted {
        name: "it's".into(),
        mode: Mode::Repeat,
        statement: "MATCH (n) WHERE n.s = 'x\\y'\nRETURN n; // x".into(),
        rate: 60,
        params: serde_json::from_value(json!({"a b": "o'k", "n": 2})).unwrap(),
    };
    assert_eq!(
        create_call(&w),
        "CALL apoc.periodic.repeat('it\\'s', 'MATCH (n) WHERE n.s = \\'x\\\\y\\'\\nRETURN n; // x', 60, {params: {`a b`: 'o\\'k', n: 2}})"
    );
    let w = Wanted {
        mode: Mode::Submit,
        params: Map::new(),
        ..w
    };
    assert_eq!(
        create_call(&w),
        "CALL apoc.periodic.submit('it\\'s', 'MATCH (n) WHERE n.s = \\'x\\\\y\\'\\nRETURN n; // x')"
    );
    let w = Wanted {
        mode: Mode::Countdown,
        rate: 5,
        ..w
    };
    assert!(create_call(&w).starts_with("CALL apoc.periodic.countdown('it\\'s', "));
    assert!(create_call(&w).ends_with(", 5)"));
    assert_eq!(
        cancel_call("a') YIELD x //"),
        "CALL apoc.periodic.cancel('a\\') YIELD x //')"
    );
}

#[test]
fn a_new_job_is_one_call_and_a_running_name_is_refused() {
    let w = wanted(&fv(
        json!({"name": "j", "rate": 10, "statement": "RETURN 1"}),
    ))
    .unwrap();
    let p = plan(None, &w, &[job("other", 5, false, false)], "neo4j").unwrap();
    assert_eq!(p.id, "apoc:j");
    assert_eq!(
        p.statements,
        ["CALL apoc.periodic.repeat('j', 'RETURN 1', 10)"]
    );
    assert!(p
        .script
        .starts_with("// Job de APOC «j» (apoc.periodic.repeat), en la base «neo4j».\n"));
    assert!(p.script.contains("no sobrevive a un reinicio"));
    assert!(p
        .script
        .ends_with("CALL apoc.periodic.repeat('j', 'RETURN 1', 10);\n"));
    // The user's home database when the connection names none.
    let p = plan(None, &w, &[], "").unwrap();
    assert!(p.script.contains("la base predeterminada"));
    // A name a running job has is refused; a finished one is replaced.
    let e = plan(None, &w, &[job("j", 5, false, false)], "neo4j").unwrap_err();
    assert!(e.to_string().contains("Ya hay"));
    // A finished job without a rate may be a countdown whose next run is
    // pending: allowed, with the warning.
    let p = plan(None, &w, &[job("j", 0, true, false)], "neo4j").unwrap();
    assert!(p.script.contains(&cypher::comment_text(MAYBE_COUNTDOWN)));
    assert!(!plan(None, &w, &[], "neo4j")
        .unwrap()
        .script
        .contains("cuenta regresiva"));
    assert!(plan(None, &w, &[job("j", 5, false, true)], "neo4j").is_ok());
}

#[test]
fn an_edit_calls_again_and_a_new_name_cancels_the_old_job() {
    let cur = job("j", 10, false, false);
    let w = wanted(&fv(
        json!({"name": "j", "type": "submit", "statement": "RETURN 1"}),
    ))
    .unwrap();
    let p = plan(Some(&cur), &w, std::slice::from_ref(&cur), "neo4j").unwrap();
    assert_eq!(p.statements, ["CALL apoc.periodic.submit('j', 'RETURN 1')"]);
    assert!(p.script.contains("cancela el anterior y programa este"));
    assert_eq!(p.id, "apoc:j");

    let w = wanted(&fv(
        json!({"name": "k", "rate": 5, "statement": "RETURN 1"}),
    ))
    .unwrap();
    let p = plan(Some(&cur), &w, std::slice::from_ref(&cur), "neo4j").unwrap();
    assert_eq!(
        p.statements,
        [
            "CALL apoc.periodic.repeat('k', 'RETURN 1', 5)",
            "CALL apoc.periodic.cancel('j')"
        ]
    );
    assert_eq!(p.id, "apoc:k");
    assert!(p.script.contains("Reemplaza al job «j»"));
    let e = plan(
        Some(&cur),
        &w,
        &[cur.clone(), job("k", 1, false, false)],
        "neo4j",
    )
    .unwrap_err();
    assert!(e.to_string().contains("Ya hay"));
}

#[test]
fn delete_cancels() {
    let p = delete_plan(&job("it's", 10, false, false)).unwrap();
    assert_eq!(p.statements, ["CALL apoc.periodic.cancel('it\\'s')"]);
    assert!(p.script.starts_with("// Cancela el job de APOC «it's»"));
    assert!(!p.script.contains("cuenta regresiva"), "{}", p.script);
}

/// A countdown as APOC lists it before its first run re-arms it.
fn countdown(name: &str, delay: i64, done: bool) -> Job {
    Job {
        delay,
        ..job(name, 0, done, false)
    }
}

#[test]
fn a_countdown_is_told_apart_and_its_scripts_say_a_cancel_doesnt_stop_it() {
    let c = countdown("down", 3, false);
    assert!(c.countdown() && !c.maybe_countdown());
    assert_eq!(c.mode(), Mode::Countdown);
    assert_eq!(c.describe(), "Cuenta regresiva: se repite cada 3 s");
    let once = job("once", 0, true, false);
    assert!(!once.countdown() && once.maybe_countdown());
    let rep = job("rep", 5, false, false);
    assert!(!rep.countdown() && !rep.maybe_countdown());

    // Delete: the cancel is honest about a countdown.
    let d = delete_plan(&c).unwrap();
    assert_eq!(d.statements, ["CALL apoc.periodic.cancel('down')"]);
    assert!(
        d.script.contains(&cypher::comment_text(COUNTDOWN)),
        "{}",
        d.script
    );
    assert!(d
        .script
        .contains("si todavía no terminó su primera ejecución"));
    let d = delete_plan(&once).unwrap();
    assert!(
        d.script.contains(&cypher::comment_text(MAYBE_COUNTDOWN)),
        "{}",
        d.script
    );

    // The same name is refused: the pending run would replace the new job.
    let w = wanted(&fv(
        json!({"name": "down", "type": "countdown", "rate": 3, "statement": "RETURN 0"}),
    ))
    .unwrap();
    let e = plan(Some(&c), &w, std::slice::from_ref(&c), "neo4j").unwrap_err();
    assert_eq!(e.to_string(), COUNTDOWN_SAME_NAME);
    // So is a new job under the name of a known countdown, done or not.
    let w2 = wanted(&fv(
        json!({"name": "down", "rate": 5, "statement": "RETURN 1"}),
    ))
    .unwrap();
    for cur in [c.clone(), countdown("down", 3, true)] {
        let e = plan(None, &w2, std::slice::from_ref(&cur), "neo4j").unwrap_err();
        assert!(e.to_string().contains("Ya hay"), "{e}");
    }
    // Another name: the old countdown's cancel is said not to stop it.
    let w3 = wanted(&fv(
        json!({"name": "down2", "type": "countdown", "rate": 3, "statement": "RETURN 0"}),
    ))
    .unwrap();
    let p = plan(Some(&c), &w3, std::slice::from_ref(&c), "neo4j").unwrap();
    assert_eq!(
        p.statements,
        [
            "CALL apoc.periodic.countdown('down2', 'RETURN 0', 3)",
            "CALL apoc.periodic.cancel('down')"
        ]
    );
    assert!(
        p.script.contains(&cypher::comment_text(COUNTDOWN)),
        "{}",
        p.script
    );
    // The same name on a job without a rate (maybe a countdown): allowed,
    // with the warning.
    let w4 = wanted(&fv(
        json!({"name": "once", "type": "submit", "statement": "RETURN 1"}),
    ))
    .unwrap();
    let p = plan(Some(&once), &w4, std::slice::from_ref(&once), "neo4j").unwrap();
    assert!(p.script.contains(&cypher::comment_text(MAYBE_COUNTDOWN)));

    // The form: its delay as the wait, the countdown notes.
    let f = form_of(Some(&c), "neo4j");
    assert_eq!(f.values["type"], json!("countdown"));
    assert_eq!(f.values["rate"], json!(3));
    assert!(f.notes.iter().any(|n| n == COUNTDOWN));
    assert!(f.notes.iter().any(|n| n == COUNTDOWN_SAME_NAME));
    assert!(!f.notes.iter().any(|n| n == REPLACE));
    // A new job's form says it too; a repeating job's doesn't need to.
    assert!(form_of(None, "neo4j").notes.iter().any(|n| n == COUNTDOWN));
    let f = form_of(Some(&rep), "neo4j");
    assert!(!f
        .notes
        .iter()
        .any(|n| n == COUNTDOWN || n == MAYBE_COUNTDOWN));
    let f = form_of(Some(&once), "neo4j");
    assert!(f.notes.iter().any(|n| n == MAYBE_COUNTDOWN));
}

#[test]
fn a_reader_only_login_is_known_not_to_schedule() {
    assert!(may_write(None));
    assert!(may_write(Some(&Value::Null)), "Community: no roles");
    assert!(may_write(Some(&json!([]))));
    assert!(!may_write(Some(&json!(["reader", "PUBLIC"]))));
    assert!(!may_write(Some(&json!(["PUBLIC"]))));
    assert!(may_write(Some(&json!(["editor", "PUBLIC"]))));
    assert!(may_write(Some(&json!(["reader", "etl"]))), "a custom role");
}

#[test]
fn every_statement_is_one_unit_of_the_script() {
    // A name with line breaks can't reach a comment line; statements with
    // `;` and comments inside strings stay one unit each.
    let w = Wanted {
        name: "x */ ; // y".into(),
        mode: Mode::Repeat,
        statement: "RETURN ';' AS a // c\n; RETURN 2".into(),
        rate: 1,
        params: Map::new(),
    };
    let p = plan(Some(&job("old\nline", 1, false, false)), &w, &[], "db").unwrap();
    assert_eq!(cypher::script(&p.script).len(), 2);
    assert!(p.script.contains("// ⚠ Reemplaza al job «old?line»"));
}

#[test]
fn jobs_are_read_from_apoc_list() {
    let r = |v: Value| Job::of(v.as_object().unwrap()).unwrap();
    let rep = r(json!({"name": "a", "rate": 30, "delay": 0, "done": false, "cancelled": false}));
    assert_eq!(rep.mode(), Mode::Repeat);
    assert_eq!(rep.describe(), "Se repite cada 30 s");
    let once = r(json!({"name": "b", "rate": 0, "delay": 0, "done": true, "cancelled": false}));
    assert_eq!(once.mode(), Mode::Submit);
    assert_eq!(once.describe(), "Una vez (terminado)");
    let c = r(json!({"name": "c", "rate": 30, "done": false, "cancelled": true}));
    assert_eq!(c.describe(), "Se repite cada 30 s (cancelado)");
    assert!(c.finished());
    // A countdown: a delay, no rate (as APOC lists it before its first
    // run re-arms it).
    let d = r(json!({"name": "d", "rate": 0, "delay": 2, "done": false, "cancelled": false}));
    assert_eq!(d.mode(), Mode::Countdown);
    assert!(Job::of(json!({"name": ""}).as_object().unwrap()).is_none());
    assert_eq!(job_id("a b"), "apoc:a b");
}

#[test]
fn the_form_brings_what_apoc_lists_and_asks_for_the_statement() {
    let f = form_of(None, "neo4j");
    assert!(!f.read_only);
    assert_eq!(f.values["type"], json!("repeat"));
    assert_eq!(f.values["rate"], json!(60));
    assert_eq!(f.values["database"], json!("neo4j"));
    assert!(!f.sections[0].fields.iter().any(|x| x.key == "done"));
    let rate = f.sections[0]
        .fields
        .iter()
        .find(|x| x.key == "rate")
        .unwrap();
    assert_eq!(
        rate.when,
        Some((
            "type".to_string(),
            vec!["repeat".to_string(), "countdown".to_string()]
        ))
    );
    let params = f.sections[1]
        .fields
        .iter()
        .find(|x| x.key == "params")
        .unwrap();
    assert_eq!(
        params.when,
        Some((
            "type".to_string(),
            vec!["repeat".to_string(), "submit".to_string()]
        ))
    );

    let f = form_of(Some(&job("j", 15, false, false)), "");
    assert_eq!(f.values["name"], json!("j"));
    assert_eq!(f.values["rate"], json!(15));
    assert_eq!(f.values["statement"], json!(""));
    // The user's default database: empty, which the field's help explains
    // (a value would never be translated).
    assert_eq!(f.values["database"], json!(""));
    let db = f.sections[0]
        .fields
        .iter()
        .find(|x| x.key == "database")
        .unwrap();
    assert!(db
        .help
        .as_deref()
        .unwrap()
        .contains("la base predeterminada del usuario"));
    assert_eq!(f.values["done"], json!(false));
    for k in ["delay", "done", "cancelled", "database"] {
        let x = f.sections[0].fields.iter().find(|x| x.key == k).unwrap();
        assert!(x.read_only, "{k}");
    }
    assert!(f.notes.iter().any(|n| n == RETYPE));
    assert!(f.notes.iter().any(|n| n == IN_MEMORY));
    // Saved as it came (the statement typed again), it is the same call.
    let mut values = f.values.clone();
    values.insert("statement".into(), json!("RETURN 1"));
    let w = wanted(&FormValues {
        values,
        lists: BTreeMap::new(),
    })
    .unwrap();
    assert_eq!(
        create_call(&w),
        "CALL apoc.periodic.repeat('j', 'RETURN 1', 15)"
    );
}

#[test]
fn only_neo4j_has_the_designer() {
    assert!(supported(Flavor::Neo4j));
    assert!(!supported(Flavor::Memgraph));
    assert!(!supported(Flavor::Neptune));
}

/// The script's warnings, as the app reads them.
fn warned(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|l| l.strip_prefix("// ⚠ "))
        .map(str::to_string)
        .collect()
}

#[test]
fn scripts_warn_about_memory_recreation_and_countdowns() {
    let w = wanted(&fv(
        json!({"name": "j", "rate": 10, "statement": "RETURN 1"}),
    ))
    .unwrap();
    // A new job: nothing to accept; that it doesn't survive a restart is
    // a plain note (every APOC job lives in memory).
    let p = plan(None, &w, &[], "neo4j").unwrap();
    assert!(warned(&p.script).is_empty(), "{}", p.script);
    assert!(p.script.contains(&format!("// {LOST_ON_RESTART}\n")));
    // A new job under the name of a finished one that may be a countdown.
    let once = job("j", 0, true, false);
    let p = plan(None, &w, std::slice::from_ref(&once), "neo4j").unwrap();
    assert_eq!(warned(&p.script), [MAYBE_COUNTDOWN]);
    // The same name: scheduled again.
    let cur = job("j", 10, false, false);
    let p = plan(Some(&cur), &w, std::slice::from_ref(&cur), "neo4j").unwrap();
    assert_eq!(warned(&p.script), [SAME_NAME]);
    assert!(p.script.contains(&format!("// {LOST_ON_RESTART}\n")));
    // Another name: two calls, each on its own.
    let k = wanted(&fv(
        json!({"name": "k", "rate": 10, "statement": "RETURN 1"}),
    ))
    .unwrap();
    let p = plan(Some(&cur), &k, std::slice::from_ref(&cur), "neo4j").unwrap();
    let got = warned(&p.script);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[0].starts_with("Reemplaza al job «j»"));
    assert_eq!(&got[1..], [NOT_ATOMIC]);
    // A countdown renamed or deleted keeps going.
    let c = countdown("down", 3, false);
    let p = plan(Some(&c), &k, std::slice::from_ref(&c), "neo4j").unwrap();
    assert!(warned(&p.script).iter().any(|x| x == COUNTDOWN));
    assert_eq!(warned(&delete_plan(&c).unwrap().script), [COUNTDOWN]);
    let once = job("once", 0, true, false);
    assert_eq!(
        warned(&delete_plan(&once).unwrap().script),
        [MAYBE_COUNTDOWN]
    );
    // A plain job's delete: nothing to accept.
    assert!(warned(&delete_plan(&cur).unwrap().script).is_empty());
}

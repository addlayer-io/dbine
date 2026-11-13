//! Unit tests of "Ejecutar con parámetros": the form a job gets, what the
//! run sends, what's refused, the script, and the calls against a mock
//! workspace.

use super::super::tests::{posts, rich, workspace, Seen};
use super::*;
use crate::transfer::tests::session;
use dbine_driver::Session;
use std::sync::{Arc, Mutex};

/// What the app sends back for a form left as it came.
fn back(f: &ObjectForm) -> FormValues {
    FormValues {
        values: f.values.clone(),
        lists: Default::default(),
    }
}

/// The rich job without its job parameters (its tasks take their own).
fn tasks_only() -> Current {
    let mut c = rich();
    c.job["settings"]
        .as_object_mut()
        .unwrap()
        .remove("parameters");
    c
}

fn err<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(Error::Query(m)) => m,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn with(c: &Current, changes: Json) -> FormValues {
    let mut v = back(&form_of(c));
    for (k, x) in changes.as_object().unwrap() {
        v.values.insert(k.clone(), x.clone());
    }
    v
}

fn keys(f: &ObjectForm, section: &str) -> Vec<String> {
    f.sections
        .iter()
        .find(|s| s.key == section)
        .map(|s| s.fields.iter().map(|x| x.key.clone()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

#[test]
fn a_job_with_job_parameters_offers_only_them() {
    let c = rich();
    let f = form_of(&c);
    assert_eq!(
        keys(&f, "job_parameters"),
        ["param:fecha", "param:modo", "new_parameters"]
    );
    assert_eq!(f.values["param:fecha"], json!("hoy"));
    assert_eq!(f.values["param:modo"], json!(""));
    let field = &f.sections[0].fields[0];
    assert_eq!(field.label, "fecha");
    // The workspace changes only the job's at run time then.
    assert!(keys(&f, "task_parameters").is_empty());
    assert_eq!(keys(&f, "run"), ["only", "full_refresh"]);
    // Its pipeline task refreshes fully already.
    assert_eq!(f.values["full_refresh"], json!(true));
    let tasks = options(&f.sections[1].fields[0].kind);
    assert_eq!(tasks.len(), 16);
    assert_eq!(tasks[0], "ingesta");
    assert_eq!(f.values["only"], json!([]));
    assert_eq!(f.values["version"], json!(version(&c)));
    assert_eq!(f.notes, [JOB_PARAMS_NOTE]);
    assert!(!f.read_only);
}

#[test]
fn a_job_without_them_offers_what_its_tasks_share() {
    let f = form_of(&tasks_only());
    assert_eq!(keys(&f, "job_parameters"), Vec::<String>::new());
    assert_eq!(
        keys(&f, "task_parameters"),
        [
            "notebook_params",
            "python_params",
            "python_named_params",
            "sql_params",
            "jar_params",
            "spark_submit_params"
        ]
    );
    // A value a line can't hold is JSON; the nested notebook of the
    // for-each task isn't one of the job's.
    let nb: Json = serde_json::from_str(f.values["notebook_params"].as_str().unwrap()).unwrap();
    assert_eq!(
        nb,
        json!({ "dia": "{{job.parameters.fecha}}", "multi": "a\nb" })
    );
    assert_eq!(
        f.values["python_params"],
        json!("--fecha\n{{job.parameters.fecha}}")
    );
    assert_eq!(f.values["python_named_params"], json!("modo=full"));
    // The query has p=1, the file none: p is theirs to share.
    assert_eq!(f.values["sql_params"], json!("p=1"));
    assert_eq!(
        f.values["spark_submit_params"],
        json!("--class\norg.apache.spark.examples.SparkPi\ndbfs:/x.jar\n10")
    );
    assert_eq!(f.notes, [TASK_PARAMS_NOTE]);

    // Tasks that disagree share nothing; a job with none of these tasks
    // gets new job parameters instead.
    let mut c = tasks_only();
    let tasks = c.job["settings"]["tasks"].as_array_mut().unwrap();
    tasks.push(json!({ "task_key": "otro_script", "spark_python_task": { "python_file": "b.py", "parameters": ["--x"] } }));
    tasks.push(json!({ "task_key": "otra", "sql_task": { "warehouse_id": "w", "query": { "query_id": "q" }, "parameters": { "p": "2", "q": "3" } } }));
    let f = form_of(&c);
    assert_eq!(f.values["python_params"], json!(""));
    assert_eq!(f.values["sql_params"], json!("q=3"));
    let mut bare = tasks_only();
    bare.job["settings"]["tasks"] = json!([{ "task_key": "si", "condition_task": { "op": "EQUAL_TO", "left": "1", "right": "1" } }]);
    let f = form_of(&bare);
    assert_eq!(keys(&f, "job_parameters"), ["new_parameters"]);
    assert_eq!(keys(&f, "run"), ["only"]);
    assert!(f.notes.is_empty());
}

// ---------------------------------------------------------------------------
// What the run sends
// ---------------------------------------------------------------------------

#[test]
fn a_form_left_as_it_came_runs_the_job_as_it_is() {
    for c in [rich(), tasks_only()] {
        let r = run_of(&c, &back(&form_of(&c))).unwrap();
        assert_eq!(r.body, json!({ "job_id": 42 }));
        assert!(r.warnings.is_empty());
        assert!(r
            .script()
            .contains("como está configurado: no cambia ningún valor"));
    }
    // Nothing but the id either.
    assert_eq!(
        run_of(&rich(), &FormValues::default()).unwrap().body,
        json!({ "job_id": 42 })
    );
}

#[test]
fn job_parameters_send_what_differs() {
    let c = rich();
    let r = run_of(
        &c,
        &with(
            &c,
            json!({ "param:fecha": "2026-11-01", "new_parameters": "extra=1\nvacio" }),
        ),
    )
    .unwrap();
    assert_eq!(
        r.body,
        json!({ "job_id": 42, "job_parameters": { "fecha": "2026-11-01", "extra": "1", "vacio": "" } })
    );
    // A number is a string to the API; a default sent back isn't sent.
    let r = run_of(
        &c,
        &with(&c, json!({ "param:modo": 3, "param:fecha": "hoy" })),
    )
    .unwrap();
    assert_eq!(r.body["job_parameters"], json!({ "modo": "3" }));

    for (changes, wanted) in [
        (json!({ "param:otro": "x" }), "no es un parámetro del job"),
        (json!({ "param:fecha": ["x"] }), "tiene que ser un texto"),
        (
            json!({ "new_parameters": "fecha=x" }),
            "ya es un parámetro del job",
        ),
        (
            json!({ "new_parameters": "a b=x" }),
            "no es un nombre válido",
        ),
        (json!({ "new_parameters": "=x" }), "no tiene clave"),
        (json!({ "notebook_params": "dia=1" }), JOB_PARAMS_ONLY),
        (json!({ "version": "otra" }), "cambió en el workspace"),
    ] {
        let m = err(run_of(&c, &with(&c, changes.clone())));
        assert!(m.contains(wanted), "{changes}: {m}");
    }
}

#[test]
fn task_parameters_send_what_changed() {
    let c = tasks_only();
    // Notebooks: only the keys that changed (merged over each one's own).
    let r = run_of(
        &c,
        &with(
            &c,
            json!({ "notebook_params": "{\"dia\": \"{{job.parameters.fecha}}\", \"multi\": \"a\\nb\", \"x\": \"1\"}" }),
        ),
    )
    .unwrap();
    assert_eq!(
        r.body,
        json!({ "job_id": 42, "notebook_params": { "x": "1" } })
    );
    assert!(r.warnings.is_empty(), "one notebook");
    // The rest: the whole value; two SQL tasks hear it: a warning.
    let r = run_of(
        &c,
        &with(
            &c,
            json!({ "sql_params": "p=2\nq=3", "python_params": "--fecha\n2026-11-01" }),
        ),
    )
    .unwrap();
    assert_eq!(
        r.body,
        json!({ "job_id": 42, "python_params": ["--fecha", "2026-11-01"], "sql_params": { "p": "2", "q": "3" } })
    );
    assert_eq!(r.warnings, [ALL_OF_A_KIND]);
    let r = run_of(&c, &with(&c, json!({ "jar_params": "[\"a\", 1]" }))).unwrap();
    assert_eq!(r.body["jar_params"], json!(["a", "1"]));

    let big = format!("x={}", "y".repeat(MAX_BYTES));
    for (changes, wanted) in [
        (json!({ "python_params": "--año" }), "solo caracteres ASCII"),
        (
            json!({ "spark_submit_params": "ñ" }),
            "solo caracteres ASCII",
        ),
        (
            json!({ "notebook_params": "x=1", "jar_params": "a" }),
            NOTEBOOK_AND_JAR,
        ),
        (json!({ "new_parameters": "a=1" }), NO_NEW_PARAMS),
        (json!({ "sql_params": big }), "pasa de los 10000 bytes"),
        (
            json!({ "sql_params": "{\"p\": [1]}" }),
            "tiene que ser un texto",
        ),
        (
            json!({ "python_params": "[1, [2]]" }),
            "tiene que ser un texto",
        ),
    ] {
        let m = err(run_of(&c, &with(&c, changes.clone())));
        assert!(m.contains(wanted), "{changes}: {m}");
    }
    // A field for tasks the job doesn't have.
    let mut bare = tasks_only();
    bare.job["settings"]["tasks"] =
        json!([{ "task_key": "nb", "notebook_task": { "notebook_path": "/x" } }]);
    let m = err(run_of(&bare, &with(&bare, json!({ "jar_params": "a" }))));
    assert!(m.contains("no tiene tareas que reciban"), "{m}");
    // Empty fields of the kinds it doesn't have are fine.
    assert!(run_of(&bare, &with(&bare, json!({ "jar_params": "" }))).is_ok());
}

#[test]
fn tasks_and_a_full_refresh() {
    let c = rich();
    // Some tasks, in the job's order; all of them is the whole job.
    let r = run_of(&c, &with(&c, json!({ "only": ["si", "ingesta"] }))).unwrap();
    assert_eq!(r.body["only"], json!(["ingesta", "si"]));
    let all = options(&form_of(&c).sections[1].fields[0].kind);
    let r = run_of(&c, &with(&c, json!({ "only": all }))).unwrap();
    assert!(r.body.get("only").is_none());
    assert!(err(run_of(&c, &with(&c, json!({ "only": ["nada"] })))).contains("no es una tarea"));

    // Its pipeline refreshes fully: turning it off is sent, no warning.
    let r = run_of(&c, &with(&c, json!({ "full_refresh": false }))).unwrap();
    assert_eq!(r.body["pipeline_params"], json!({ "full_refresh": false }));
    assert!(r.warnings.is_empty());
    // One that doesn't: turning it on is, with a warning.
    let mut p = rich();
    for t in p.job["settings"]["tasks"].as_array_mut().unwrap() {
        if let Some(pt) = t.get_mut("pipeline_task") {
            pt["full_refresh"] = json!(false);
        }
    }
    let r = run_of(&p, &with(&p, json!({ "full_refresh": "true" }))).unwrap();
    assert_eq!(r.body["pipeline_params"], json!({ "full_refresh": true }));
    assert_eq!(r.warnings, [FULL_REFRESH]);
    assert!(r.script().contains(&format!("-- ⚠ {FULL_REFRESH}\n")));
    // No pipelines: nothing to refresh.
    let mut bare = rich();
    bare.job["settings"]["tasks"] =
        json!([{ "task_key": "nb", "notebook_task": { "notebook_path": "/x" } }]);
    assert_eq!(
        err(run_of(&bare, &with(&bare, json!({ "full_refresh": true })))),
        NO_PIPELINES
    );
    assert!(run_of(&bare, &with(&bare, json!({ "full_refresh": false }))).is_ok());
}

/// A multiple choice's values.
fn options(k: &FormKind) -> Vec<String> {
    match k {
        FormKind::MultiSelect { options } => options.iter().map(|(v, _)| v.clone()).collect(),
        k => panic!("{k:?}"),
    }
}

#[test]
fn the_script_is_comments_whatever_the_values() {
    let c = rich();
    let r = run_of(
        &c,
        &with(
            &c,
            json!({ "param:fecha": "a\nb\u{2028}c\r-- DROP", "only": ["ingesta"] }),
        ),
    )
    .unwrap();
    let sc = r.script();
    assert!(sc.starts_with(
        "-- Databricks Jobs API: la solicitud que DBine envía al ejecutar el job.\n-- El editor de SQL no la envía: va comentada.\n--\n-- Ejecuta ahora el job «dbine_test_sched carga» (42) con estos valores; lo demás, como está configurado.\n-- POST /api/2.1/jobs/run-now\n"
    ), "{sc}");
    assert!(sc.lines().all(|l| l.starts_with("--")), "{sc}");
    let body: Json = serde_json::from_str(
        &sc.lines()
            .skip_while(|l| !l.starts_with("-- {"))
            .map(|l| l.strip_prefix("-- ").unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    assert_eq!(body, r.body);
}

// ---------------------------------------------------------------------------
// Against a mock workspace
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_action_runs_the_job_with_its_values() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let mut s = session(workspace(seen.clone(), true).await);
    let f = s.scheduler_action_form("job", "42", ACTION).await.unwrap();
    assert_eq!(f.values["param:fecha"], json!("hoy"));
    let mut v = back(&f);
    v.values.insert("param:fecha".into(), json!("ayer"));
    let script = s
        .scheduler_action_script("job", "42", ACTION, &v)
        .await
        .unwrap();
    assert!(
        script.contains("-- POST /api/2.1/jobs/run-now\n"),
        "{script}"
    );
    assert!(posts(&seen).is_empty(), "the script sends nothing");
    s.scheduler_action("job", "42", ACTION, &v).await.unwrap();
    assert_eq!(
        posts(&seen),
        [(
            "POST".to_string(),
            "/api/2.1/jobs/run-now".to_string(),
            json!({ "job_id": 42, "job_parameters": { "fecha": "ayer" } })
        )]
    );
    // A value the job wouldn't take: refused before the run.
    v.values.insert("param:nada".into(), json!("x"));
    assert!(s.scheduler_action("job", "42", ACTION, &v).await.is_err());
    assert_eq!(posts(&seen).len(), 1);
}

#[tokio::test]
async fn bad_ids_kinds_and_actions_never_reach_the_workspace() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let mut s = session(workspace(seen.clone(), true).await);
    let v = FormValues::default();
    for bad in [
        "42/../7",
        "42&job_id=7",
        "-42",
        "x",
        "",
        "99999999999999999999",
    ] {
        assert!(s.scheduler_action_form("job", bad, ACTION).await.is_err());
        assert!(s
            .scheduler_action_script("job", bad, ACTION, &v)
            .await
            .is_err());
        assert!(s.scheduler_action("job", bad, ACTION, &v).await.is_err());
    }
    for kind in ["schedule", "../job", ""] {
        assert!(s.scheduler_action_form(kind, "42", ACTION).await.is_err());
        assert!(s.scheduler_action(kind, "42", ACTION, &v).await.is_err());
    }
    for action in ["run", "backfill", ""] {
        assert!(matches!(
            s.scheduler_action_form("job", "42", action).await,
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            s.scheduler_action("job", "42", action, &v).await,
            Err(Error::Unsupported(_))
        ));
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );
}

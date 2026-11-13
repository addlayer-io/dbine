//! Unit tests of the scheduler's forms, settings, plans and scripts (no
//! workspace; the API calls against a local mock).

use super::lines::*;
use super::plan::{self, Method, Plan};
use super::*;
use crate::transfer::tests::{serve, session};
use dbine_driver::{FormValues, ObjectForm, Session};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn sources(destinations: bool) -> Sources {
    let mut s = Sources::default();
    s.lists.insert(
        "warehouse".into(),
        vec![("abc123".into(), "Starter Warehouse".into())],
    );
    if destinations {
        s.lists.insert(
            "destination".into(),
            vec![
                ("dest-1".into(), "Slack (SLACK)".into()),
                ("dest-2".into(), "PagerDuty (PAGERDUTY)".into()),
            ],
        );
    }
    s
}

/// A job with every kind of task and every part of the settings, as
/// `jobs/get` gives it.
fn rich_job() -> Json {
    let mut j = json!({
        "job_id": 42,
        "creator_user_name": "ana@example.com",
        "run_as_user_name": "1234-abcd",
        "created_time": 1762916400000_i64,
        "settings": {
            "name": "dbine_test_sched carga",
            "description": "ventas\n al lakehouse ",
            "tags": { "equipo": "datos", "pii": "" },
            "run_as": { "service_principal_name": "1234-abcd" },
            "edit_mode": "EDITABLE",
            "performance_target": "STANDARD",
            "budget_policy_id": "bp-1",
            "format": "MULTI_TASK",
            "usage_policy_id": "up-9",
            "schedule": {
                "quartz_cron_expression": "0 0 3 * * ?",
                "timezone_id": "America/Argentina/Buenos_Aires",
                "pause_status": "PAUSED"
            },
            "max_concurrent_runs": 2,
            "queue": { "enabled": true },
            "timeout_seconds": 7200,
            "health": { "rules": [{ "metric": "RUN_DURATION_SECONDS", "op": "GREATER_THAN", "value": 3600 }] },
            "parameters": [{ "name": "fecha", "default": "hoy" }, { "name": "modo", "default": "" }],
            "email_notifications": {
                "on_failure": ["ana@example.com", "ops@example.com"],
                "no_alert_for_skipped_runs": false
            },
            "webhook_notifications": { "on_start": [{ "id": "dest-1" }] },
            "notification_settings": { "no_alert_for_canceled_runs": true },
            "git_source": { "git_url": "https://github.com/empresa/etl", "git_provider": "gitHub", "git_branch": "main" },
            "job_clusters": [{
                "job_cluster_key": "jc",
                "new_cluster": {
                    "spark_version": "15.4.x-scala2.12",
                    "node_type_id": "i3.xlarge",
                    "num_workers": 2,
                    "spark_conf": { "spark.sql.shuffle.partitions": "8" },
                    "aws_attributes": { "availability": "SPOT_WITH_FALLBACK", "zone_id": "auto" },
                    "data_security_mode": "SINGLE_USER",
                    "single_user_name": "ana@example.com",
                    "runtime_engine": "PHOTON"
                }
            }],
            "environments": [{
                "environment_key": "default",
                "spec": { "environment_version": "2", "dependencies": ["pandas==2.2.2"], "java_dependencies": ["/Volumes/m/l/a.jar"] }
            }]
        }
    });
    j["settings"]["tasks"] = json!(rich_tasks());
    j
}

/// The rich job's tasks, one of each kind.
fn rich_tasks() -> Vec<Json> {
    vec![
        json!({
                    "task_key": "ingesta",
                    "description": "lee los archivos",
                    "notebook_task": {
                        "notebook_path": "/Repos/etl/ingesta",
                        "source": "WORKSPACE",
                        "base_parameters": { "dia": "{{job.parameters.fecha}}", "multi": "a\nb" }
                    },
                    "job_cluster_key": "jc",
                    "libraries": [
                        { "pypi": { "package": "requests==2.32.3", "repo": "https://pypi.example.com/simple" } },
                        { "maven": { "coordinates": "com.example:lib:1.0", "exclusions": ["org.slf4j:slf4j-api"] } },
                        { "whl": "/Volumes/main/libs/w/etl-1.0-py3-none-any.whl" },
                        { "requirements": "/Workspace/etl/requirements.txt" },
                        { "jar": "/Volumes/main/libs/j/with space.jar" }
                    ],
                    "timeout_seconds": 600,
                    "max_retries": 3,
                    "min_retry_interval_millis": 60000,
                    "retry_on_timeout": true,
                    "email_notifications": { "on_success": ["ana@example.com"] },
                    "notification_settings": { "alert_on_last_attempt": true },
                    "run_if": "ALL_SUCCESS"
        }),
        json!({
                    "task_key": "script",
                    "depends_on": [{ "task_key": "ingesta" }],
                    "run_if": "ALL_DONE",
                    "spark_python_task": { "python_file": "etl/limpia.py", "source": "GIT", "parameters": ["--fecha", "{{job.parameters.fecha}}"] },
                    "environment_key": "default"
        }),
        json!({
                    "task_key": "wheel",
                    "depends_on": [{ "task_key": "script" }],
                    "python_wheel_task": { "package_name": "etl", "entry_point": "main", "named_parameters": { "modo": "full" } },
                    "existing_cluster_id": "0101-123456-abcdefgh"
        }),
        json!({
                    "task_key": "jar",
                    "spark_jar_task": { "main_class_name": "com.example.Carga", "parameters": ["  con espacios  "], "run_as_repl": true },
                    "new_cluster": {
                        "spark_version": "15.4.x-scala2.12",
                        "node_type_id": "i3.xlarge",
                        "autoscale": { "min_workers": 1, "max_workers": 8 },
                        "init_scripts": [{ "volumes": { "destination": "/Volumes/main/s/init.sh" } }]
                    }
        }),
        json!({
                    "task_key": "submit",
                    "spark_submit_task": { "parameters": ["--class", "org.apache.spark.examples.SparkPi", "dbfs:/x.jar", "10"] },
                    "new_cluster": { "spark_version": "15.4.x-scala2.12", "node_type_id": "i3.xlarge", "num_workers": 0, "custom_tags": { "ResourceClass": "SingleNode" } }
        }),
        json!({
                    "task_key": "consulta",
                    "sql_task": { "warehouse_id": "abc123", "query": { "query_id": "q-1" }, "parameters": { "p": "1" } }
        }),
        json!({
                    "task_key": "archivo",
                    "sql_task": { "warehouse_id": "abc123", "file": { "path": "sql/modelo.sql", "source": "GIT" } }
        }),
        json!({
                    "task_key": "tablero",
                    "sql_task": {
                        "warehouse_id": "abc123",
                        "dashboard": { "dashboard_id": "d-1", "custom_subject": "Ventas", "pause_subscriptions": true,
                                       "subscriptions": [{ "user_name": "ana@example.com" }, { "destination_id": "dest-2" }] }
                    }
        }),
        json!({
                    "task_key": "alerta",
                    "sql_task": { "warehouse_id": "abc123", "alert": { "alert_id": "a-1", "subscriptions": [{ "user_name": "ops@example.com" }] } }
        }),
        json!({
                    "task_key": "dbt",
                    "dbt_task": { "commands": ["dbt deps", "dbt run --select ventas"], "project_directory": "dbt/", "warehouse_id": "abc123", "schema": "ventas", "catalog": "main" },
                    "existing_cluster_id": "0101-123456-abcdefgh",
                    "libraries": [{ "pypi": { "package": "dbt-databricks>=1.8.0,<2.0" } }]
        }),
        json!({
                    "task_key": "pipeline",
                    "pipeline_task": { "pipeline_id": "0f7c2c8e-1111-2222-3333-444455556666", "full_refresh": true }
        }),
        json!({
                    "task_key": "otro_job",
                    "run_job_task": { "job_id": 12, "job_parameters": { "fecha": "{{job.parameters.fecha}}" } }
        }),
        json!({
                    "task_key": "hay_datos",
                    "depends_on": [{ "task_key": "consulta" }],
                    "condition_task": { "op": "GREATER_THAN", "left": "{{tasks.consulta.values.n}}", "right": "0" }
        }),
        json!({
                    "task_key": "si",
                    "depends_on": [{ "task_key": "hay_datos", "outcome": "true" }, { "task_key": "archivo" }],
                    "for_each_task": {
                        "inputs": "[\"a\", \"b\"]",
                        "concurrency": 2,
                        "task": { "task_key": "si_iteracion", "notebook_task": { "notebook_path": "/x", "base_parameters": { "v": "{{input}}" } } }
                    }
        }),
        json!({
                    "task_key": "no",
                    "depends_on": [{ "task_key": "hay_datos", "outcome": "false" }],
                    "dashboard_task": {
                        "dashboard_id": "01ef-dash",
                        "warehouse_id": "abc123",
                        "subscription": { "custom_subject": "Tablero", "paused": false, "subscribers": [{ "user_name": "ana@example.com" }] }
                    }
        }),
        json!({
                    "task_key": "power_bi",
                    "power_bi_task": { "connection_resource_name": "pbi", "power_bi_model": { "workspace_name": "w", "model_name": "m" } },
                    "disabled": true,
                    "health": { "rules": [{ "metric": "RUN_DURATION_SECONDS", "op": "GREATER_THAN", "value": 60 }] },
                    "webhook_notifications": { "on_failure": [{ "id": "dest-2" }] }
        }),
    ]
}

fn rich_acl() -> Vec<Json> {
    vec![
        json!({ "user_name": "ana@example.com", "display_name": "Ana", "all_permissions": [{ "permission_level": "IS_OWNER", "inherited": false }] }),
        json!({ "group_name": "datos", "all_permissions": [{ "permission_level": "CAN_MANAGE_RUN", "inherited": false }] }),
        json!({ "group_name": "admins", "all_permissions": [{ "permission_level": "CAN_MANAGE", "inherited": true, "inherited_from_object": ["/jobs/"] }] }),
    ]
}

fn rich() -> Current {
    Current {
        id: 42,
        job: rich_job(),
        acl: Some(rich_acl()),
    }
}

/// What the app sends back for a form left as it came.
fn back(f: &ObjectForm) -> FormValues {
    FormValues {
        values: f.values.clone(),
        lists: f
            .sections
            .iter()
            .flat_map(|s| s.lists.iter())
            .map(|l| (l.key.clone(), l.rows.clone()))
            .collect(),
    }
}

fn row<'a>(
    v: &'a mut FormValues,
    list: &str,
    title: &str,
    key: &str,
) -> &'a mut BTreeMap<String, Json> {
    v.lists
        .get_mut(list)
        .unwrap()
        .iter_mut()
        .find(|r| r.get(key).and_then(Json::as_str) == Some(title))
        .unwrap_or_else(|| panic!("{list}: no row {title}"))
}

fn task<'a>(v: &'a mut FormValues, key: &str) -> &'a mut BTreeMap<String, Json> {
    row(v, "tasks", key, "task_key")
}

fn err(r: Result<impl std::fmt::Debug>) -> String {
    match r {
        Err(Error::Query(m)) => m,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// The only request of a plan, as (method, path, body).
fn only(p: &Plan) -> (Method, &str, &Json) {
    assert_eq!(p.requests.len(), 1, "{:#?}", p.requests);
    let r = &p.requests[0];
    (r.method, r.path.as_str(), &r.body)
}

// ---------------------------------------------------------------------------
// Ids, values, kinds
// ---------------------------------------------------------------------------

#[test]
fn ids_keys_and_kinds_are_checked() {
    assert!(check_kind("job").is_ok());
    assert!(check_kind(" job ").is_ok());
    for bad in ["task", "Job", "", "job;"] {
        assert!(check_kind(bad).is_err(), "{bad}");
    }
    assert_eq!(checked_id(" abc-123_x.y ", "x").unwrap(), "abc-123_x.y");
    for bad in ["", "a/b", "a b", "../x", "x?y=1", "a\nb", &"x".repeat(129)] {
        assert!(checked_id(bad, "x").is_err(), "{bad:?}");
    }
    assert_eq!(checked_key("carga_1-a", "Tareas").unwrap(), "carga_1-a");
    for bad in ["", "con espacio", "a.b", "ñandú", "a/b", &"k".repeat(101)] {
        assert!(checked_key(bad, "Tareas").is_err(), "{bad:?}");
    }
}

#[test]
fn values_read_whatever_json_type_they_come_as() {
    let m: BTreeMap<String, Json> = serde_json::from_value(json!({
        "n": "12", "f": 3.0, "b": "true", "t": 1, "s": "  x  ", "arr": ["a", " b ", "", "a"],
        "lines": "x\ny, z", "nul": null, "bad": "1.5"
    }))
    .unwrap();
    let v = Vals(&m);
    assert_eq!(v.int("n", "N", 0, 100).unwrap(), Some(12));
    assert_eq!(v.int("f", "F", 0, 100).unwrap(), Some(3));
    assert_eq!(v.int("nul", "N", 0, 100).unwrap(), None);
    assert!(v.int("n", "N", 0, 5).is_err());
    assert!(v.int("bad", "B", 0, 5).is_err());
    assert!(v.bool("b") && v.bool("t") && !v.bool("nul"));
    assert_eq!(v.raw("s"), "  x  ");
    assert_eq!(v.text("s"), "x");
    assert_eq!(v.strings("arr"), ["a", "b"]);
    assert_eq!(v.strings("lines"), ["x", "y", "z"]);
}

// ---------------------------------------------------------------------------
// Text formats
// ---------------------------------------------------------------------------

#[test]
fn maps_are_lines_unless_a_line_would_change_them() {
    let m = json!({ "a": "1", "b": "x=y", "pii": "" });
    let text = render_map(m.as_object().unwrap());
    assert_eq!(text, "a=1\nb=x=y\npii=");
    assert_eq!(
        parse_map(&text, "L").unwrap().unwrap(),
        *m.as_object().unwrap()
    );
    assert_eq!(
        parse_map("a = 1\n\n pii \n", "L").unwrap().unwrap(),
        *json!({ "a": "1", "pii": "" }).as_object().unwrap()
    );
    for lossy in [
        json!({ "multi": "a\nb" }),
        json!({ "k=": "v" }),
        json!({ "sp": " v " }),
        json!({ "n": 1 }),
    ] {
        let t = render_map(lossy.as_object().unwrap());
        assert!(t.starts_with('{'), "{t}");
        assert_eq!(
            parse_map(&t, "L").unwrap().unwrap(),
            *lossy.as_object().unwrap()
        );
    }
    assert_eq!(parse_map("  ", "L").unwrap(), None);
    assert!(parse_map("=v", "L").is_err());
    assert!(parse_map("a=1\na=2", "L").is_err());
    assert!(parse_map("{\"a\": {\"b\": 1}}", "L").is_err());
    assert!(parse_map("{nope", "L").is_err());
}

#[test]
fn arguments_are_lines_unless_one_would_change() {
    let a = vec![json!("--fecha"), json!("{{job.parameters.fecha}}")];
    let t = render_args(&a);
    assert_eq!(t, "--fecha\n{{job.parameters.fecha}}");
    assert_eq!(parse_args(&t, "A").unwrap().unwrap(), a);
    for lossy in [
        vec![json!("  x  ")],
        vec![json!("a\nb")],
        vec![json!("")],
        vec![json!("[x")],
    ] {
        let t = render_args(&lossy);
        assert!(t.starts_with('['), "{t}");
        assert_eq!(parse_args(&t, "A").unwrap().unwrap(), lossy);
    }
    assert!(parse_args("[{\"a\": 1}]", "A").is_err());
    assert_eq!(parse_args("", "A").unwrap(), None);
}

#[test]
fn libraries_as_lines() {
    for (lib, line) in [
        (
            json!({ "pypi": { "package": "pandas==2.2.2" } }),
            "pypi:pandas==2.2.2",
        ),
        (
            json!({ "pypi": { "package": "x", "repo": "https://r.example.com/simple" } }),
            "pypi:x repo=https://r.example.com/simple",
        ),
        (
            json!({ "maven": { "coordinates": "g:a:1", "exclusions": ["x:y", "z:w"] } }),
            "maven:g:a:1 exclusions=x:y,z:w",
        ),
        (
            json!({ "cran": { "package": "forecast" } }),
            "cran:forecast",
        ),
        (json!({ "jar": "/Volumes/a/b.jar" }), "jar:/Volumes/a/b.jar"),
        (json!({ "whl": "/Workspace/w.whl" }), "whl:/Workspace/w.whl"),
        (json!({ "egg": "dbfs:/e.egg" }), "egg:dbfs:/e.egg"),
        (
            json!({ "requirements": "/Workspace/r.txt" }),
            "requirements:/Workspace/r.txt",
        ),
    ] {
        assert_eq!(render_library(&lib), line);
        assert_eq!(parse_library(line).unwrap(), lib, "{line}");
    }
    // What a line can't say goes as JSON, both ways.
    for lib in [
        json!({ "pypi": { "package": "x", "extra": 1 } }),
        json!({ "cran": { "package": "x", "exclusions": ["a"] } }),
        json!({ "jar": " spaced.jar " }),
        json!({ "future": { "x": 1 } }),
    ] {
        let line = render_library(&lib);
        assert!(line.starts_with('{'), "{line}");
        assert_eq!(parse_library(&line).unwrap(), lib);
    }
    assert!(parse_library("nuget:x").is_err());
    assert!(parse_library("pypi:").is_err());
    assert!(parse_library("pypi:x foo=bar").is_err());
    assert!(parse_library("cran:x exclusions=a").is_err());
    assert!(parse_library("{}").is_err());
    assert_eq!(parse_libraries("\n").unwrap(), None);
}

#[test]
fn rules_subscribers_and_mail() {
    let r = json!({ "metric": "RUN_DURATION_SECONDS", "op": "GREATER_THAN", "value": 3600 });
    assert_eq!(render_rule(&r), "RUN_DURATION_SECONDS > 3600");
    assert_eq!(
        parse_rules("run_duration_seconds > 3600", "R").unwrap(),
        [r]
    );
    let odd = json!({ "metric": "RUN_DURATION_SECONDS", "op": "LESS_THAN", "value": 1 });
    assert_eq!(parse_rules(&render_rule(&odd), "R").unwrap(), [odd]);
    assert!(parse_rules("CPU > 3", "R").is_err());
    assert!(parse_rules("RUN_DURATION_SECONDS > -1", "R").is_err());
    assert!(parse_rules("RUN_DURATION_SECONDS 3", "R").is_err());

    let subs = [
        json!({ "user_name": "ana@example.com" }),
        json!({ "destination_id": "d-1" }),
    ];
    let text: Vec<String> = subs.iter().map(render_subscriber).collect();
    assert_eq!(text, ["ana@example.com", "destination:d-1"]);
    assert_eq!(parse_subscribers(&text.join("\n"), "S").unwrap(), subs);
    assert_eq!(
        parse_subscribers("user:ops@example.com", "S").unwrap(),
        [json!({ "user_name": "ops@example.com" })]
    );
    assert!(parse_subscribers("destination:../x", "S").is_err());
    assert!(parse_subscribers("dos palabras", "S").is_err());

    assert_eq!(
        parse_emails("a@x.com, b@x.com\nb@x.com; c@x.com", "M").unwrap(),
        ["a@x.com", "b@x.com", "c@x.com"]
    );
    assert!(parse_emails("no-es-correo", "M").is_err());
    assert!(parse_emails("a b@x.com", "M").is_err());
}

// ---------------------------------------------------------------------------
// The form and the settings it gives back
// ---------------------------------------------------------------------------

#[test]
fn an_unchanged_form_changes_nothing() {
    for dest in [true, false] {
        let cur = rich();
        let f = form::form(Some(&cur), &sources(dest), None);
        let v = back(&f);
        let want = spec::settings(&v, Some(cur.settings())).unwrap();
        for k in cur.settings().as_object().unwrap().keys() {
            if k == "format" {
                continue;
            }
            assert_eq!(
                plan::norm(&want[k]),
                plan::norm(&cur.settings()[k]),
                "{k} (destinations listed: {dest})"
            );
        }
        let p = plan::change(&cur, &v).unwrap();
        assert!(p.requests.is_empty(), "{:#?}", p.requests);
        assert_eq!(
            p.script(),
            "-- «dbine_test_sched carga»: no hay cambios que guardar.\n"
        );
    }
}

#[test]
fn the_form_shows_the_job_as_the_workspace_has_it() {
    let cur = rich();
    let f = form::form(Some(&cur), &sources(true), None);
    let keys: Vec<&str> = f.sections.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "general",
            "tasks",
            "schedule",
            "parameters",
            "compute",
            "notifications",
            "git",
            "permissions",
            "advanced"
        ]
    );
    let v = &f.values;
    assert_eq!(v["job_id"], "42");
    assert_eq!(v["creator"], "ana@example.com");
    assert_eq!(v["created"], "2025-11-12T03:00:00Z");
    assert_eq!(v["run_as_mode"], "service_principal");
    assert_eq!(v["run_as_name"], "1234-abcd");
    assert_eq!(v["tags"], "equipo=datos\npii=");
    assert_eq!(v["trigger_mode"], "schedule");
    assert_eq!(v["paused"], true);
    assert_eq!(v["cron"], "0 0 3 * * ?");
    assert_eq!(v["hook_on_start"], json!(["dest-1"]));
    assert_eq!(v["email_on_failure"], "ana@example.com\nops@example.com");
    assert_eq!(v["git_ref_kind"], "branch");
    assert_eq!(v["git_ref"], "main");
    assert_eq!(v["settings_extra"], "{\n  \"usage_policy_id\": \"up-9\"\n}");
    assert!(v["version"].as_str().is_some_and(|s| s.len() == 24));
    assert_eq!(v["permissions_read"], true);

    let lists: BTreeMap<&str, &dbine_driver::FormList> = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .map(|l| (l.key.as_str(), l))
        .collect();
    let tasks = &lists["tasks"].rows;
    let t = |k: &str| tasks.iter().find(|r| r["task_key"] == k).unwrap();
    let types: Vec<&str> = tasks.iter().map(|r| r["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "notebook",
            "python_script",
            "python_wheel",
            "jar",
            "spark_submit",
            "sql_query",
            "sql_file",
            "sql_dashboard",
            "sql_alert",
            "dbt",
            "pipeline",
            "run_job",
            "condition",
            "for_each",
            "dashboard",
            "other"
        ]
    );
    assert_eq!(t("ingesta")["compute"], "job_cluster");
    assert_eq!(
        t("ingesta")["libraries"],
        "pypi:requests==2.32.3 repo=https://pypi.example.com/simple\nmaven:com.example:lib:1.0 exclusions=org.slf4j:slf4j-api\nwhl:/Volumes/main/libs/w/etl-1.0-py3-none-any.whl\nrequirements:/Workspace/etl/requirements.txt\njar:/Volumes/main/libs/j/with space.jar"
    );
    assert!(t("ingesta")["base_parameters"]
        .as_str()
        .unwrap()
        .starts_with('{'));
    assert_eq!(t("script")["compute"], "serverless");
    assert_eq!(t("script")["environment_key"], "default");
    assert_eq!(t("script")["depends_on"], json!(["ingesta"]));
    assert_eq!(t("wheel")["compute"], "existing");
    assert_eq!(t("jar")["compute"], "new_cluster_autoscale");
    assert_eq!(t("jar")["autoscale_max"], 8);
    assert!(t("jar")["cluster_extra"]
        .as_str()
        .unwrap()
        .contains("init_scripts"));
    assert!(t("jar")["task_extra"].as_str().unwrap().is_empty());
    assert_eq!(t("submit")["compute"], "new_cluster");
    assert_eq!(t("submit")["num_workers"], 0);
    assert_eq!(
        t("tablero")["subscriptions"],
        "ana@example.com\ndestination:dest-2"
    );
    assert_eq!(t("si")["depends_true"], json!(["hay_datos"]));
    assert_eq!(t("si")["depends_on"], json!(["archivo"]));
    assert_eq!(t("no")["depends_false"], json!(["hay_datos"]));
    assert_eq!(t("otro_job")["run_job_id"], 12);
    assert!(t("power_bi")["task_extra"]
        .as_str()
        .unwrap()
        .contains("power_bi_task"));
    assert_eq!(t("power_bi")["health"], "RUN_DURATION_SECONDS > 60");
    assert_eq!(t("power_bi")["hook_on_failure"], json!(["dest-2"]));
    assert_eq!(lists["job_clusters"].rows[0]["workers"], "fixed");
    assert!(lists["job_clusters"].rows[0]["cluster_extra"]
        .as_str()
        .unwrap()
        .contains("aws_attributes"));
    assert!(lists["environments"].rows[0]["env_extra"]
        .as_str()
        .unwrap()
        .contains("java_dependencies"));
    assert_eq!(lists["parameters"].rows.len(), 2);
    assert_eq!(lists["health"].rows[0]["value"], 3600);
    assert_eq!(lists["permissions"].rows.len(), 2);
    assert!(!lists["permissions"].read_only);
    assert_eq!(
        lists["inherited_permissions"].rows[0]["principal"],
        "admins"
    );
    assert!(lists["inherited_permissions"].read_only);
    assert!(!f.read_only);
    assert!(f.choices.contains_key("warehouse"));
}

#[test]
fn every_trigger_reads_back_unchanged() {
    for (settings, mode) in [
        (json!({ "name": "j" }), "none"),
        (
            json!({ "name": "j", "schedule": { "quartz_cron_expression": "0 */5 * * * ?", "timezone_id": "UTC" } }),
            "schedule",
        ),
        (
            json!({ "name": "j", "trigger": { "pause_status": "UNPAUSED", "periodic": { "interval": 4, "unit": "HOURS" } } }),
            "periodic",
        ),
        (
            json!({ "name": "j", "trigger": { "pause_status": "PAUSED", "file_arrival": { "url": "s3://b/in/", "min_time_between_triggers_seconds": 60, "wait_after_last_change_seconds": 0 } } }),
            "file_arrival",
        ),
        (
            json!({ "name": "j", "trigger": { "table_update": { "table_names": ["main.a.b", "main.a.c"], "condition": "ALL_UPDATED", "wait_after_last_change_seconds": 120 } } }),
            "table_update",
        ),
        (
            json!({ "name": "j", "trigger": { "table": { "table_names": ["main.a.b"], "condition": "ANY_UPDATED" } } }),
            "table_update",
        ),
        (
            json!({ "name": "j", "continuous": { "pause_status": "UNPAUSED", "task_retry_mode": "ON_FAILURE" } }),
            "continuous",
        ),
        (
            json!({ "name": "j", "trigger": { "pause_status": "PAUSED", "model": { "condition": "MODEL_CREATED" } } }),
            "other",
        ),
    ] {
        let cur = Current {
            id: 1,
            job: json!({ "job_id": 1, "settings": settings }),
            acl: None,
        };
        let f = form::form(Some(&cur), &sources(false), None);
        assert_eq!(f.values["trigger_mode"], mode);
        let p = plan::change(&cur, &back(&f)).unwrap();
        assert!(p.requests.is_empty(), "{mode}: {:#?}", p.requests);
        if mode == "other" {
            assert!(f
                .notes
                .iter()
                .any(|n| n.contains("disparador que DBine no conoce")));
        }
    }
}

#[test]
fn a_new_job_is_one_create_with_everything() {
    let f = form::form(None, &sources(true), None);
    assert!(!f.values.contains_key("version"));
    let mut v = back(&f);
    v.values
        .insert("name".into(), json!("dbine_test_sched nuevo"));
    v.values.insert("trigger_mode".into(), json!("schedule"));
    v.values.insert("cron".into(), json!("0  0 3 * * ?"));
    v.values.insert("paused".into(), json!(true));
    v.values.insert("tags".into(), json!("equipo=datos"));
    v.values
        .insert("email_on_failure".into(), json!("ana@example.com"));
    v.values.insert("hook_on_failure".into(), json!(["dest-1"]));
    v.lists.insert(
        "tasks".into(),
        vec![
            serde_json::from_value(json!({
                "task_key": "a", "type": "condition", "condition_op": "EQUAL_TO",
                "condition_left": "1", "condition_right": "1", "run_if": "ALL_SUCCESS", "compute": "serverless"
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "task_key": "b", "type": "sql_query", "warehouse_id": "abc123", "query_id": "q-9",
                "depends_true": ["a"], "max_retries": 2
            }))
            .unwrap(),
        ],
    );
    v.lists.insert(
        "permissions".into(),
        vec![serde_json::from_value(
            json!({ "principal_type": "group", "principal": "datos", "level": "CAN_VIEW" }),
        )
        .unwrap()],
    );
    let p = plan::create(&v).unwrap();
    let (m, path, body) = only(&p);
    assert_eq!((m, path), (Method::Post, "/api/2.1/jobs/create"));
    assert_eq!(
        *body,
        json!({
            "name": "dbine_test_sched nuevo",
            "tags": { "equipo": "datos" },
            "schedule": { "quartz_cron_expression": "0 0 3 * * ?", "timezone_id": "UTC", "pause_status": "PAUSED" },
            "max_concurrent_runs": 1,
            "queue": { "enabled": true },
            "email_notifications": { "on_failure": ["ana@example.com"] },
            "webhook_notifications": { "on_failure": [{ "id": "dest-1" }] },
            "tasks": [
                { "task_key": "a", "run_if": "ALL_SUCCESS",
                  "condition_task": { "op": "EQUAL_TO", "left": "1", "right": "1" } },
                { "task_key": "b", "depends_on": [{ "task_key": "a", "outcome": "true" }],
                  "sql_task": { "warehouse_id": "abc123", "query": { "query_id": "q-9" } }, "max_retries": 2 }
            ],
            "access_control_list": [{ "group_name": "datos", "permission_level": "CAN_VIEW" }]
        })
    );
    let script = p.script();
    assert!(script.starts_with(
        "-- Databricks Jobs API: las solicitudes que DBine envía al guardar, en este orden.\n"
    ));
    assert!(script.contains("\n-- Crea el job «dbine_test_sched nuevo».\n-- Su propietario es quien lo crea.\n-- POST /api/2.1/jobs/create\n-- {\n"));
    assert!(script.lines().all(|l| l.starts_with("--")), "{script}");
}

#[test]
fn an_edit_sends_only_what_changed() {
    let cur = rich();
    let base = back(&form::form(Some(&cur), &sources(true), None));

    // The name and one task: update with those.
    let mut v = base.clone();
    v.values.insert("name".into(), json!("carga nueva"));
    task(&mut v, "consulta").insert("query_id".into(), json!("q-2"));
    let p = plan::change(&cur, &v).unwrap();
    let (m, path, body) = only(&p);
    assert_eq!((m, path), (Method::Post, "/api/2.1/jobs/update"));
    assert_eq!(
        *body,
        json!({
            "job_id": 42,
            "new_settings": {
                "name": "carga nueva",
                "tasks": [{ "task_key": "consulta", "sql_task": { "warehouse_id": "abc123", "query": { "query_id": "q-2" }, "parameters": { "p": "1" } } }]
            }
        })
    );

    // A removed task and a cleared schedule: fields_to_remove.
    let mut v = base.clone();
    v.lists
        .get_mut("tasks")
        .unwrap()
        .retain(|r| r["task_key"] != "power_bi");
    v.values.insert("trigger_mode".into(), json!("none"));
    v.values.insert("description".into(), json!(""));
    let p = plan::change(&cur, &v).unwrap();
    let (_, _, body) = only(&p);
    assert_eq!(body["new_settings"], json!({}));
    assert_eq!(
        body["fields_to_remove"],
        json!(["description", "schedule", "tasks/power_bi"])
    );

    // A rename: the new key, the old one removed, and the task that
    // depends on it (still naming it as it was) follows.
    let mut v = base.clone();
    task(&mut v, "ingesta").insert("task_key".into(), json!("lectura"));
    let p = plan::change(&cur, &v).unwrap();
    let (_, _, body) = only(&p);
    let changed: Vec<&str> = body["new_settings"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task_key"].as_str().unwrap())
        .collect();
    assert_eq!(changed, ["lectura", "script"]);
    assert_eq!(
        body["new_settings"]["tasks"][1]["depends_on"],
        json!([{ "task_key": "lectura" }])
    );
    // Its cluster, libraries and the rest went with it.
    assert_eq!(body["new_settings"]["tasks"][0]["job_cluster_key"], "jc");
    assert_eq!(
        body["new_settings"]["tasks"][0]["libraries"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    assert_eq!(body["fields_to_remove"], json!(["tasks/ingesta"]));

    // A job cluster's size: that cluster whole, the rest of its spec kept.
    let mut v = base.clone();
    row(&mut v, "job_clusters", "jc", "job_cluster_key").insert("num_workers".into(), json!(4));
    let p = plan::change(&cur, &v).unwrap();
    let (_, _, body) = only(&p);
    let jc = &body["new_settings"]["job_clusters"][0];
    assert_eq!(jc["new_cluster"]["num_workers"], 4);
    assert_eq!(
        jc["new_cluster"]["aws_attributes"]["availability"],
        "SPOT_WITH_FALLBACK"
    );
    assert!(body["new_settings"].get("tasks").is_none());

    // Pausing: the schedule whole.
    let mut v = base.clone();
    v.values.insert("paused".into(), json!(false));
    let p = plan::change(&cur, &v).unwrap();
    let (_, _, body) = only(&p);
    assert_eq!(
        body["new_settings"],
        json!({ "schedule": { "quartz_cron_expression": "0 0 3 * * ?", "timezone_id": "America/Argentina/Buenos_Aires", "pause_status": "UNPAUSED" } })
    );

    // A trigger instead of the schedule.
    let mut v = base.clone();
    v.values
        .insert("trigger_mode".into(), json!("file_arrival"));
    v.values
        .insert("file_url".into(), json!("/Volumes/main/in/"));
    v.values.insert("min_between".into(), json!(120));
    let p = plan::change(&cur, &v).unwrap();
    let (_, _, body) = only(&p);
    assert_eq!(
        body["new_settings"],
        json!({ "trigger": { "pause_status": "PAUSED", "file_arrival": { "url": "/Volumes/main/in/", "min_time_between_triggers_seconds": 120 } } })
    );
    assert_eq!(body["fields_to_remove"], json!(["schedule"]));
}

#[test]
fn parameters_and_environments_go_by_reset() {
    let cur = rich();
    let base = back(&form::form(Some(&cur), &sources(true), None));
    let mut v = base.clone();
    v.lists.get_mut("parameters").unwrap().pop();
    let p = plan::change(&cur, &v).unwrap();
    let (m, path, body) = only(&p);
    assert_eq!((m, path), (Method::Post, "/api/2.1/jobs/reset"));
    assert_eq!(body["job_id"], 42);
    assert_eq!(
        body["new_settings"]["parameters"],
        json!([{ "name": "fecha", "default": "hoy" }])
    );
    // The whole settings, as the workspace has them otherwise.
    assert_eq!(body["new_settings"]["tasks"].as_array().unwrap().len(), 16);
    assert_eq!(body["new_settings"]["usage_policy_id"], "up-9");
    assert!(body["new_settings"].get("format").is_none());
    let script = p.script();
    assert!(script.contains("-- Cambian los parámetros o los entornos serverless del job, de los que jobs/update no puede\n"));

    let mut v = base;
    row(&mut v, "environments", "default", "environment_key")
        .insert("dependencies".into(), json!("pandas==2.2.3"));
    let p = plan::change(&cur, &v).unwrap();
    let (_, path, _) = only(&p);
    assert_eq!(path, "/api/2.1/jobs/reset");
}

#[test]
fn permissions_go_after_the_settings() {
    let cur = rich();
    let base = back(&form::form(Some(&cur), &sources(true), None));
    let mut v = base.clone();
    v.values.insert("description".into(), json!("otra"));
    v.lists.get_mut("permissions").unwrap().push(
        serde_json::from_value(json!({ "principal_type": "user", "principal": "bea@example.com", "level": "CAN_MANAGE" })).unwrap(),
    );
    let p = plan::change(&cur, &v).unwrap();
    assert_eq!(p.requests.len(), 2);
    assert_eq!(p.requests[0].path, "/api/2.1/jobs/update");
    assert_eq!(p.requests[1].method, Method::Put);
    assert_eq!(p.requests[1].path, "/api/2.0/permissions/jobs/42");
    assert_eq!(
        p.requests[1].body,
        json!({ "access_control_list": [
            { "user_name": "ana@example.com", "permission_level": "IS_OWNER" },
            { "group_name": "datos", "permission_level": "CAN_MANAGE_RUN" },
            { "user_name": "bea@example.com", "permission_level": "CAN_MANAGE" }
        ] })
    );
    assert!(p
        .script()
        .contains("\n-- PUT /api/2.0/permissions/jobs/42\n"));

    // The owner can't go away, nor be two, nor a group.
    let mut v = base.clone();
    v.lists
        .get_mut("permissions")
        .unwrap()
        .retain(|r| r["level"] != "IS_OWNER");
    assert!(err(plan::change(&cur, &v)).contains("necesita un propietario"));
    let mut v = base.clone();
    v.lists.get_mut("permissions").unwrap().push(
        serde_json::from_value(json!({ "principal_type": "user", "principal": "bea@example.com", "level": "IS_OWNER" })).unwrap(),
    );
    assert!(err(plan::change(&cur, &v)).contains("un solo propietario"));
    let mut v = base.clone();
    row(&mut v, "permissions", "datos", "principal").insert("level".into(), json!("IS_OWNER"));
    assert!(err(plan::change(&cur, &v)).contains("no un grupo"));

    // Permissions the form couldn't read stay as they are.
    let mut v = base;
    v.values.insert("permissions_read".into(), json!(false));
    v.lists.insert("permissions".into(), vec![]);
    assert!(plan::change(&cur, &v).unwrap().requests.is_empty());
}

#[test]
fn the_form_checks_what_the_api_would_refuse() {
    let cur = rich();
    let base = back(&form::form(Some(&cur), &sources(true), None));
    let check = |change: &dyn Fn(&mut FormValues), want: &str| {
        let mut v = base.clone();
        change(&mut v);
        let m = err(plan::change(&cur, &v));
        assert!(m.contains(want), "{m:?} lacks {want:?}");
    };
    check(
        &|v| {
            v.values.insert("name".into(), json!("  "));
        },
        "necesita un nombre",
    );
    check(
        &|v| {
            v.values.insert("cron".into(), json!("0 3 * *"));
        },
        "no es una expresión de Quartz",
    );
    check(
        &|v| {
            v.values.insert("cron".into(), json!("0 0 3 * * ?; drop"));
        },
        "no es una expresión de Quartz",
    );
    check(
        &|v| {
            v.values.insert("timezone".into(), json!("Mi zona"));
        },
        "no es una zona horaria",
    );
    check(
        &|v| {
            v.values.insert("max_concurrent_runs".into(), json!(1001));
        },
        "entre 0 y 1000",
    );
    check(
        &|v| {
            v.values.insert(
                "tags".into(),
                json!((0..26)
                    .map(|i| format!("t{i}=x"))
                    .collect::<Vec<_>>()
                    .join("\n")),
            );
        },
        "hasta 25 etiquetas",
    );
    check(
        &|v| {
            v.values
                .insert("settings_extra".into(), json!("{\"name\": \"x\"}"));
        },
        "no puede tener «name»",
    );
    check(
        &|v| {
            v.values.insert("settings_extra".into(), json!("[1]"));
        },
        "tiene que ser un objeto JSON",
    );
    check(
        &|v| {
            v.values.insert("run_as_mode".into(), json!("user"));
            v.values.insert("run_as_name".into(), json!(""));
        },
        "falta el usuario",
    );
    check(
        &|v| {
            v.values.insert("git_provider".into(), json!(null));
        },
        "falta el proveedor",
    );
    check(
        &|v| {
            v.values.insert("trigger_mode".into(), json!("other"));
        },
        "solo conserva el disparador",
    );
    check(
        &|v| {
            v.values
                .insert("trigger_mode".into(), json!("file_arrival"));
            v.values.insert("file_url".into(), json!("s3://x/"));
            v.values.insert("wait_after".into(), json!(30));
        },
        "al menos 60 segundos",
    );
    check(
        &|v| {
            v.values
                .insert("trigger_mode".into(), json!("table_update"));
            v.values.insert("table_names".into(), json!(""));
        },
        "al menos una tabla",
    );
    check(
        &|v| {
            v.lists.get_mut("parameters").unwrap()[1].insert("name".into(), json!("fecha"));
        },
        "está dos veces",
    );
    check(
        &|v| {
            v.lists.get_mut("parameters").unwrap()[1].insert("name".into(), json!("con espacio"));
        },
        "no es un nombre válido",
    );
    check(
        &|v| {
            task(v, "script").insert("task_key".into(), json!("wheel"));
        },
        "está dos veces",
    );
    check(
        &|v| {
            task(v, "script").insert("task_key".into(), json!("mal nombre"));
        },
        "no es una clave válida",
    );
    check(
        &|v| {
            task(v, "script").insert("depends_on".into(), json!(["fantasma"]));
        },
        "que no está en el job",
    );
    check(
        &|v| {
            task(v, "script").insert("depends_on".into(), json!(["script"]));
        },
        "no puede depender de sí misma",
    );
    check(
        &|v| {
            task(v, "ingesta").insert("depends_on".into(), json!(["wheel"]));
        },
        "forman un ciclo",
    );
    check(
        &|v| {
            task(v, "si").insert("depends_true".into(), json!(["consulta"]));
        },
        "no es una tarea de condición",
    );
    check(
        &|v| {
            task(v, "ingesta").insert("notebook_path".into(), json!(""));
        },
        "falta «Notebook»",
    );
    check(
        &|v| {
            task(v, "wheel").insert("parameters".into(), json!("--x"));
        },
        "no los dos",
    );
    check(
        &|v| {
            task(v, "dbt").insert("dbt_commands".into(), json!("dbt run\nrm -rf /"));
        },
        "no empieza con «dbt»",
    );
    check(
        &|v| {
            task(v, "jar").insert("autoscale_min".into(), json!(9));
        },
        "es mayor que el máximo",
    );
    check(
        &|v| {
            task(v, "wheel").insert("existing_cluster_id".into(), json!("../../2.0/x"));
        },
        "no es un id válido",
    );
    check(
        &|v| {
            task(v, "ingesta").insert("job_cluster_key".into(), json!("otro"));
        },
        "que no está en el job",
    );
    check(
        &|v| {
            task(v, "script").insert("environment_key".into(), json!("otro"));
        },
        "que no está en el job",
    );
    check(
        &|v| {
            task(v, "otro_job").insert("run_job_id".into(), json!("12&x=1"));
        },
        "no es un id de job",
    );
    check(
        &|v| {
            task(v, "consulta").insert("warehouse_id".into(), json!(null));
        },
        "falta «SQL warehouse»",
    );
    check(
        &|v| {
            task(v, "si").insert("foreach_task".into(), json!("{\"x\": 1}"));
        },
        "necesita su «task_key»",
    );
    check(
        &|v| {
            task(v, "power_bi").insert("task_extra".into(), json!("{}"));
        },
        "es de otro tipo",
    );
    check(
        &|v| {
            task(v, "ingesta").insert("task_extra".into(), json!("{\"sql_task\": {}}"));
        },
        "no puede tener «sql_task»",
    );
    check(
        &|v| {
            task(v, "ingesta").insert("compute".into(), json!("gpu"));
        },
        "no es un cómputo válido",
    );
    check(
        &|v| {
            task(v, "ingesta").insert("type".into(), json!("shell"));
        },
        "no es un tipo de tarea",
    );
    check(
        &|v| {
            task(v, "ingesta").insert("email_on_start".into(), json!("nadie"));
        },
        "no es una dirección de correo",
    );
    check(
        &|v| {
            task(v, "ingesta").insert("hook_on_start".into(), json!(["a/b"]));
        },
        "no es un id válido",
    );
    check(
        &|v| {
            task(v, "ingesta").insert("run_if".into(), json!("SOMETIMES"));
        },
        "no es un valor válido",
    );
    check(
        &|v| {
            row(v, "job_clusters", "jc", "job_cluster_key")
                .insert("cluster_extra".into(), json!("{\"num_workers\": 3}"));
        },
        "no puede tener «num_workers»",
    );
    check(
        &|v| {
            row(v, "permissions", "datos", "principal")
                .insert("principal_type".into(), json!("role"));
        },
        "no es un tipo válido",
    );
}

#[test]
fn run_as_comes_from_the_job_when_the_settings_leave_it_out() {
    let mut j = rich_job();
    j["settings"].as_object_mut().unwrap().remove("run_as");
    let cur = Current {
        id: 42,
        job: j.clone(),
        acl: Some(rich_acl()),
    };
    let f = form::form(Some(&cur), &sources(true), None);
    assert_eq!(f.values["run_as_mode"], "service_principal");
    assert_eq!(f.values["run_as_name"], "1234-abcd");
    let base = back(&f);
    assert!(plan::change(&cur, &base).unwrap().requests.is_empty());
    // A reset keeps it.
    let mut v = base.clone();
    v.lists.get_mut("parameters").unwrap().pop();
    let p = plan::change(&cur, &v).unwrap();
    assert_eq!(
        p.requests[0].body["new_settings"]["run_as"],
        json!({ "service_principal_name": "1234-abcd" })
    );
    // Back to the owner: the setting goes.
    let mut v = base;
    v.values.insert("run_as_mode".into(), json!("owner"));
    let p = plan::change(&cur, &v).unwrap();
    assert_eq!(p.requests[0].body["fields_to_remove"], json!(["run_as"]));
    // Running as its owner is the owner's mode.
    j["run_as_user_name"] = json!("ana@example.com");
    let cur = Current {
        id: 42,
        job: j,
        acl: Some(rich_acl()),
    };
    let f = form::form(Some(&cur), &sources(true), None);
    assert_eq!(f.values["run_as_mode"], "owner");
    assert!(plan::change(&cur, &back(&f)).unwrap().requests.is_empty());
}

#[test]
fn a_rename_of_a_cluster_or_an_environment_carries_its_users() {
    let cur = rich();
    let mut v = back(&form::form(Some(&cur), &sources(true), None));
    row(&mut v, "job_clusters", "jc", "job_cluster_key")
        .insert("job_cluster_key".into(), json!("jc2"));
    let p = plan::change(&cur, &v).unwrap();
    let (_, _, body) = only(&p);
    assert_eq!(
        body["new_settings"]["job_clusters"][0]["job_cluster_key"],
        "jc2"
    );
    assert_eq!(body["new_settings"]["tasks"][0]["job_cluster_key"], "jc2");
    assert_eq!(body["fields_to_remove"], json!(["job_clusters/jc"]));
}

#[test]
fn the_script_is_comments_whatever_the_values() {
    let cur = rich();
    let mut v = back(&form::form(Some(&cur), &sources(true), None));
    v.values.insert(
        "description".into(),
        json!("línea 1\nSELECT 1; */ DROP TABLE x;\u{2028}-- y\r\nfin"),
    );
    v.values.insert("name".into(), json!("nombre\nraro"));
    let p = plan::change(&cur, &v).unwrap();
    let script = p.script();
    assert!(script.lines().all(|l| l.starts_with("--")), "{script}");
    assert!(!script.contains('\u{2028}'));
    assert!(script.contains("\\u2028"));
    assert!(
        script.contains("-- Cambia el job «dbine_test_sched carga» (42): solo lo que cambió.\n")
    );
    // The body sent is the same JSON the script shows.
    let shown: String = script
        .lines()
        .skip_while(|l| !l.starts_with("-- {"))
        .map(|l| l.trim_start_matches("-- ").to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let back: Json = serde_json::from_str(&shown).unwrap();
    assert_eq!(back, p.requests[0].body);

    let d = plan::delete(&cur).script();
    assert_eq!(
        d,
        "-- Databricks Jobs API: las solicitudes que DBine envía al guardar, en este orden.\n\
         -- El editor de SQL no las envía: van comentadas.\n\
         -- ⚠ Se borra también su historial de ejecuciones.\n\
         --\n\
         -- Borra el job «dbine_test_sched carga» (42).\n\
         -- POST /api/2.1/jobs/delete\n\
         -- {\n\
         --   \"job_id\": 42\n\
         -- }\n"
    );
}

#[test]
fn starting_at_a_task_runs_it_and_what_comes_after() {
    let s = &rich_job()["settings"];
    assert_eq!(
        plan::from_task(s, "ingesta").unwrap(),
        ["ingesta", "script", "wheel"]
    );
    assert_eq!(
        plan::from_task(s, "CONSULTA").unwrap(),
        ["consulta", "hay_datos", "si", "no"]
    );
    assert_eq!(plan::from_task(s, "archivo").unwrap(), ["archivo", "si"]);
    assert!(err(plan::from_task(s, "nada")).contains("no tiene una tarea llamada «nada»"));
}

#[test]
fn a_form_without_its_lists_writes_ids_by_hand() {
    let cur = rich();
    let f = form::form(Some(&cur), &Sources::default(), None);
    let tasks = f.sections[1].lists[0].fields.clone();
    let kind = |k: &str| tasks.iter().find(|x| x.key == k).unwrap().kind.clone();
    assert_eq!(kind("warehouse_id"), FormKind::Text);
    assert_eq!(kind("hook_on_start"), FormKind::Textarea);
    assert_eq!(kind("depends_on"), FormKind::reference("list:tasks", true));
    assert_eq!(f.values["hook_on_start"], "dest-1");
    // And reads them back the same.
    assert!(plan::change(&cur, &back(&f)).unwrap().requests.is_empty());
}

#[test]
fn who_may_change_the_job() {
    let cur = rich();
    let me = |user: &str, groups: &[&str]| json!({ "userName": user, "groups": groups.iter().map(|g| json!({ "display": g })).collect::<Vec<_>>() });
    let ro = |me: Option<Json>, acl: Option<Vec<Json>>| {
        let c = Current { acl, ..cur.clone() };
        form::form(Some(&c), &sources(true), me.as_ref()).read_only
    };
    assert!(
        !ro(Some(me("ana@example.com", &[])), Some(rich_acl())),
        "the owner"
    );
    assert!(
        !ro(Some(me("bea@example.com", &["admins"])), Some(rich_acl())),
        "an admin"
    );
    assert!(
        ro(Some(me("bea@example.com", &["datos"])), Some(rich_acl())),
        "CAN_MANAGE_RUN only"
    );
    assert!(
        !ro(None, Some(rich_acl())),
        "unknown login: the API decides"
    );
    // A group that may hold the login's group: the API decides too.
    let mut acl = rich_acl();
    acl.push(json!({ "group_name": "ingenieria", "all_permissions": [{ "permission_level": "CAN_MANAGE", "inherited": false }] }));
    assert!(
        !ro(Some(me("bea@example.com", &["datos"])), Some(acl)),
        "a group grant it may have"
    );
    let f = form::form(
        Some(&Current {
            acl: None,
            ..cur.clone()
        }),
        &sources(true),
        None,
    );
    assert!(!f.read_only);
    assert!(f
        .notes
        .iter()
        .any(|n| n.contains("No se pudieron leer los permisos")));
    let perms = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .find(|l| l.key == "permissions")
        .unwrap();
    assert!(perms.read_only);
    // A bundle's job says so.
    let mut j = rich_job();
    j["settings"]["deployment"] = json!({ "kind": "BUNDLE", "metadata_file_path": "/x" });
    let f = form::form(Some(&Current { job: j, ..cur }), &sources(true), None);
    assert!(f.notes.iter().any(|n| n.contains("bundle")));
}

// ---------------------------------------------------------------------------
// Against a mock workspace
// ---------------------------------------------------------------------------

type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

/// A workspace with job 42 (the rich one), a few lists, and a log of what
/// was asked. `admin`: SCIM `Me` lists `admins`.
async fn workspace(seen: Seen, admin: bool) -> String {
    let job = Arc::new(Mutex::new(rich_job()));
    serve(Arc::new(move |m, p, b| {
        seen.lock().unwrap().push((m.to_string(), p.to_string(), b.to_string()));
        let (path, query) = p.split_once('?').unwrap_or((p, ""));
        let body = match (m, path) {
            ("GET", "/api/2.1/jobs/list") => json!({
                "jobs": [
                    { "job_id": 42, "settings": rich_job()["settings"].clone() },
                    { "job_id": 7, "settings": { "name": "a mano" } }
                ],
                "has_more": false
            }),
            ("GET", "/api/2.1/jobs/get") if query == "job_id=42" => job.lock().unwrap().clone(),
            ("GET", "/api/2.0/permissions/jobs/42") => json!({ "access_control_list": rich_acl() }),
            ("GET", "/api/2.0/preview/scim/v2/Me") => {
                let groups = if admin { json!([{ "display": "admins" }]) } else { json!([]) };
                json!({ "userName": "ana@example.com", "groups": groups })
            }
            ("GET", "/api/2.0/sql/warehouses") => json!({ "warehouses": [{ "id": "abc123", "name": "Starter" }] }),
            ("GET", "/api/2.1/clusters/list") => json!({ "clusters": [
                { "cluster_id": "0101-123456-abcdefgh", "cluster_name": "compartido", "cluster_source": "UI" },
                { "cluster_id": "job-1", "cluster_name": "job", "cluster_source": "JOB" }
            ] }),
            ("GET", "/api/2.0/notification-destinations") => json!({ "results": [{ "id": "dest-1", "display_name": "Slack", "destination_type": "SLACK" }] }),
            ("GET", "/api/2.1/clusters/list-node-types") => json!({ "node_types": [{ "node_type_id": "i3.xlarge", "num_cores": 4.0, "memory_mb": 31232 }] }),
            ("GET", "/api/2.1/jobs/runs/list") => {
                // Two pages of finished runs, then (once deleted) none.
                let gone = seen.lock().unwrap().iter().filter(|c| c.1 == "/api/2.1/jobs/runs/delete").count();
                if gone >= 3 {
                    json!({ "runs": [], "has_more": false })
                } else if query.contains("page_token=p2") {
                    json!({ "runs": [{ "run_id": 3 }], "has_more": false })
                } else {
                    json!({ "runs": [{ "run_id": 1 }, { "run_id": 2 }], "has_more": true, "next_page_token": "p2" })
                }
            }
            ("POST", "/api/2.1/jobs/create") => json!({ "job_id": 99 }),
            ("POST", "/api/2.1/jobs/update") => {
                let b: Json = serde_json::from_str(b).unwrap();
                if let Some(n) = b.pointer("/new_settings/name") {
                    job.lock().unwrap()["settings"]["name"] = n.clone();
                }
                json!({})
            }
            ("PUT", "/api/2.0/permissions/jobs/42") => {
                return (403, json!({ "error_code": "PERMISSION_DENIED", "message": "no CAN_MANAGE" }).to_string())
            }
            ("POST", "/api/2.1/jobs/delete" | "/api/2.1/jobs/run-now" | "/api/2.1/jobs/runs/delete") => json!({}),
            _ => return (404, json!({ "error_code": "NOT_FOUND", "message": "no" }).to_string()),
        };
        (200, body.to_string())
    }))
    .await
}

fn posts(seen: &Seen) -> Vec<(String, String, Json)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|c| c.0 != "GET")
        .map(|c| {
            (
                c.0.clone(),
                c.1.clone(),
                serde_json::from_str(&c.2).unwrap_or(Json::Null),
            )
        })
        .collect()
}

#[tokio::test]
async fn info_and_items() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let mut s = session(workspace(seen.clone(), false).await);
    let info = s.scheduler_info().await.unwrap();
    assert_eq!(info.kinds.len(), 1);
    let k = &info.kinds[0];
    assert_eq!(
        (k.key.as_str(), k.label.as_str(), k.singular.as_str()),
        ("job", "Jobs", "job")
    );
    assert!(k.can_create && k.can_edit && k.can_delete);
    assert!(!info.logs);
    assert_eq!(info.notes, [NOT_ADMIN]);
    let list = s.scheduler_items("job").await.unwrap();
    let items: Vec<(&str, &str, Option<bool>)> = list
        .items
        .iter()
        .map(|i| (i.id.as_str(), i.name.as_str(), i.enabled))
        .collect();
    // Sorted by name; a job without a schedule has no enabled state.
    assert_eq!(
        items,
        [
            ("7", "a mano", None),
            ("42", "dbine_test_sched carga", Some(false))
        ]
    );
    assert!(s.scheduler_items("alert").await.is_err());

    let admin = session(workspace(Arc::new(Mutex::new(Vec::new())), true).await)
        .scheduler_info()
        .await
        .unwrap();
    assert!(admin.notes.is_empty());

    let refused = serve(Arc::new(|_, _, _| {
        (
            403,
            json!({ "error_code": "PERMISSION_DENIED", "message": "Invalid scope" }).to_string(),
        )
    }))
    .await;
    let mut s = session(refused);
    let info = s.scheduler_info().await.unwrap();
    assert!(!info.kinds[0].can_create && !info.kinds[0].can_edit && !info.kinds[0].can_delete);
    assert_eq!(
        info.notes,
        ["Databricks no deja usar la API de jobs con estas credenciales: Invalid scope"]
    );
    assert!(s.scheduler_items("job").await.unwrap().items.is_empty());
}

#[tokio::test]
async fn the_form_reads_the_job_and_the_lists() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let mut s = session(workspace(seen, true).await);
    let f = s.scheduler_form("job", Some("42")).await.unwrap();
    assert!(!f.read_only);
    assert_eq!(
        f.choices["cluster"],
        [("0101-123456-abcdefgh".to_string(), "compartido".to_string())]
    );
    assert_eq!(f.choices["node_type"][0].1, "i3.xlarge · 4 núcleos · 30 GB");
    // Lists the mock doesn't have: a note each, and their ids by hand.
    for n in [
        "No se pudo leer la lista de pipelines del workspace: su id se escribe a mano.",
        "No se pudo leer la lista de pools de instancias: su id se escribe a mano.",
    ] {
        assert!(f.notes.iter().any(|x| x == n), "{:?}", f.notes);
    }
    let new = s.scheduler_form("job", None).await.unwrap();
    assert_eq!(new.values["trigger_mode"], "none");
    assert_eq!(new.values["max_concurrent_runs"], 1);
    assert!(new.sections[1].lists[0].rows.is_empty());
    for bad in ["../42", "42&x=1", "-1", "x"] {
        assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad}");
    }
}

#[tokio::test]
async fn saving_creating_and_deleting() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let mut s = session(workspace(seen.clone(), true).await);
    let f = s.scheduler_form("job", Some("42")).await.unwrap();
    let mut v = back(&f);
    v.values.insert("name".into(), json!("renombrado"));
    let script = s.scheduler_script("job", Some("42"), &v).await.unwrap();
    assert!(script.contains("-- POST /api/2.1/jobs/update\n"));
    assert!(posts(&seen).is_empty(), "the script sends nothing");
    assert_eq!(
        s.scheduler_save("job", Some("42"), &v)
            .await
            .unwrap()
            .as_deref(),
        Some("42")
    );
    assert_eq!(
        posts(&seen)[0],
        (
            "POST".to_string(),
            "/api/2.1/jobs/update".to_string(),
            json!({ "job_id": 42, "new_settings": { "name": "renombrado" } })
        )
    );
    // The job changed: the same form is now stale.
    assert!(err(s.scheduler_script("job", Some("42"), &v).await).contains("cambió en el workspace"));

    // Settings saved, permissions refused: said so.
    let f = s.scheduler_form("job", Some("42")).await.unwrap();
    let mut v = back(&f);
    v.values.insert("name".into(), json!("otra vez"));
    v.lists.get_mut("permissions").unwrap().pop();
    let m = err(s.scheduler_save("job", Some("42"), &v).await);
    assert_eq!(
        m,
        "Se guardaron los ajustes del job, pero no sus permisos: no CAN_MANAGE"
    );

    // A new job: its id comes back.
    let mut v = back(&s.scheduler_form("job", None).await.unwrap());
    v.values
        .insert("name".into(), json!("dbine_test_sched nuevo"));
    assert_eq!(
        s.scheduler_save("job", None, &v).await.unwrap().as_deref(),
        Some("99")
    );
    assert_eq!(posts(&seen).last().unwrap().1, "/api/2.1/jobs/create");

    let d = s.scheduler_delete_script("job", "42").await.unwrap();
    assert!(d.contains("-- POST /api/2.1/jobs/delete\n"));
    s.scheduler_delete("job", "42").await.unwrap();
    assert_eq!(posts(&seen).last().unwrap().2, json!({ "job_id": 42 }));
}

#[tokio::test]
async fn run_from_a_task_and_clear_the_history() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let mut s = session(workspace(seen.clone(), true).await);
    s.run_job_from("42", "hay_datos").await.unwrap();
    assert_eq!(
        posts(&seen)[0].2,
        json!({ "job_id": 42, "only": ["hay_datos", "si", "no"] })
    );
    assert!(s.run_job_from("42", "nada").await.is_err());

    s.clear_job_history("42").await.unwrap();
    let deleted: Vec<Json> = posts(&seen)
        .iter()
        .filter(|p| p.1 == "/api/2.1/jobs/runs/delete")
        .map(|p| p.2["run_id"].clone())
        .collect();
    assert_eq!(deleted, [json!(1), json!(2), json!(3)]);
    let asked: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.1.starts_with("/api/2.1/jobs/runs/list"))
        .map(|c| c.1.clone())
        .collect();
    assert!(asked
        .iter()
        .all(|q| q.contains("job_id=42") && q.contains("completed_only=true")));
}

#[tokio::test]
async fn bad_ids_and_kinds_never_reach_the_workspace() {
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
        assert!(s.scheduler_script("job", Some(bad), &v).await.is_err());
        assert!(s.scheduler_save("job", Some(bad), &v).await.is_err());
        assert!(s.scheduler_delete_script("job", bad).await.is_err());
        assert!(s.scheduler_delete("job", bad).await.is_err());
        assert!(s.run_job_from(bad, "ingesta").await.is_err());
        assert!(s.clear_job_history(bad).await.is_err());
    }
    for kind in ["schedule", "../job", ""] {
        assert!(s.scheduler_form(kind, None).await.is_err());
        assert!(s.scheduler_script(kind, None, &v).await.is_err());
        assert!(s.scheduler_delete(kind, "42").await.is_err());
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );
}

// ---------------------------------------------------------------------------
// Flags, what each job allows and fields per type
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_job_starts_at_a_task_and_its_finished_runs_are_deleted() {
    let mut s = session(workspace(Arc::new(Mutex::new(Vec::new())), true).await);
    let info = s.scheduler_info().await.unwrap();
    let k = &info.kinds[0];
    assert!(k.run_from_step && k.clear_history);
    // "Ejecutar ahora" is the jobs list's run-now: no action of its own.
    assert!(k.actions.is_empty());
    let refused = serve(Arc::new(|_, _, _| {
        (
            403,
            json!({ "error_code": "PERMISSION_DENIED", "message": "Invalid scope" }).to_string(),
        )
    }))
    .await;
    let info = session(refused).scheduler_info().await.unwrap();
    assert!(!info.kinds[0].run_from_step && !info.kinds[0].clear_history);
}

/// A workspace whose jobs 1-4 the login (`bea`, in no admin group unless
/// `admin`) sees: it manages 1; 2 shows its permissions without a grant to
/// it; 3 refuses them; 4 fails.
async fn shared_workspace(seen: Seen, admin: bool) -> String {
    serve(Arc::new(move |m, p, _| {
        seen.lock().unwrap().push((m.to_string(), p.to_string(), String::new()));
        let path = p.split_once('?').map_or(p, |(a, _)| a);
        let grant = |who: &str, level: &str| {
            json!({ "access_control_list": [
                { "user_name": who, "all_permissions": [{ "permission_level": level, "inherited": false }] },
                { "group_name": "admins", "all_permissions": [{ "permission_level": "CAN_MANAGE", "inherited": true }] }
            ] })
        };
        let body = match path {
            "/api/2.1/jobs/list" => json!({
                "jobs": (1..=4).map(|n| json!({ "job_id": n, "settings": { "name": format!("job {n}") } })).collect::<Vec<_>>(),
                "has_more": false
            }),
            "/api/2.0/preview/scim/v2/Me" => {
                let groups = if admin { json!([{ "display": "admins" }]) } else { json!([{ "display": "datos" }]) };
                json!({ "userName": "bea@example.com", "groups": groups })
            }
            "/api/2.0/permissions/jobs/1" => grant("bea@example.com", "CAN_MANAGE"),
            "/api/2.0/permissions/jobs/2" => grant("ana@example.com", "IS_OWNER"),
            "/api/2.0/permissions/jobs/3" => {
                return (403, json!({ "error_code": "PERMISSION_DENIED", "message": "no CAN_MANAGE" }).to_string())
            }
            _ => return (500, json!({ "error_code": "INTERNAL_ERROR", "message": "boom" }).to_string()),
        };
        (200, body.to_string())
    }))
    .await
}

#[tokio::test]
async fn items_say_which_jobs_the_login_may_change() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let mut s = session(shared_workspace(seen.clone(), false).await);
    let list = s.scheduler_items("job").await.unwrap();
    let can: Vec<(&str, Option<bool>, Option<bool>)> = list
        .items
        .iter()
        .map(|i| (i.id.as_str(), i.can_edit, i.can_delete))
        .collect();
    assert_eq!(
        can,
        [
            ("1", None, None),
            ("2", Some(false), Some(false)),
            ("3", Some(false), Some(false)),
            // A failure says nothing: as the kind says, the API decides.
            ("4", None, None),
        ]
    );
    assert!(list.items.iter().all(|i| i.actions.is_none()));
    let asked = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.1.starts_with("/api/2.0/permissions/jobs/"))
        .count();
    assert_eq!(asked, 4);

    // An admin: no permissions read, everything as the kind says.
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let mut s = session(shared_workspace(seen.clone(), true).await);
    let list = s.scheduler_items("job").await.unwrap();
    assert!(list
        .items
        .iter()
        .all(|i| i.can_edit.is_none() && i.can_delete.is_none()));
    assert!(!seen
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.1.starts_with("/api/2.0/permissions")));
}

#[tokio::test]
async fn a_job_whose_permissions_are_refused_has_a_read_only_form() {
    // The rich job, whose permissions the workspace won't show this login.
    let job = rich_job();
    let base = serve(Arc::new(move |_, p, _| {
        let (path, _) = p.split_once('?').unwrap_or((p, ""));
        match path {
            "/api/2.1/jobs/get" => (200, job.to_string()),
            "/api/2.0/permissions/jobs/42" => (
                403,
                json!({ "error_code": "PERMISSION_DENIED", "message": "no" }).to_string(),
            ),
            "/api/2.0/preview/scim/v2/Me" => (
                200,
                json!({ "userName": "bea@example.com", "groups": [] }).to_string(),
            ),
            _ => (
                404,
                json!({ "error_code": "NOT_FOUND", "message": "no" }).to_string(),
            ),
        }
    }))
    .await;
    let f = session(base)
        .scheduler_form("job", Some("42"))
        .await
        .unwrap();
    assert!(f.read_only);
    assert!(f.notes[0].contains("no tiene CAN_MANAGE"), "{:?}", f.notes);
    assert!(!f
        .notes
        .iter()
        .any(|n| n.contains("No se pudieron leer los permisos")));
}

#[test]
fn a_refused_permissions_read_means_no_manage_unless_an_admin() {
    let admin = json!({ "userName": "x", "groups": [{ "display": "admins" }] });
    let user = json!({ "userName": "x", "groups": [] });
    assert_eq!(form::may_manage(None, true, Some(&admin)), Some(true));
    assert_eq!(form::may_manage(None, true, Some(&user)), Some(false));
    assert_eq!(form::may_manage(None, true, None), Some(false));
    assert_eq!(form::may_manage(None, false, Some(&user)), None);
    assert_eq!(form::may_manage(None, false, None), None);
    let cur = Current {
        acl: None,
        ..rich()
    };
    let f = form::form_with(Some(&cur), &sources(true), None, true);
    assert!(f.read_only);
    let f = form::form_with(Some(&cur), &sources(true), None, false);
    assert!(!f.read_only);
}

#[test]
fn libraries_and_the_dedicated_user_show_only_where_they_apply() {
    let f = form::form(None, &sources(true), None);
    let tasks = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .find(|l| l.key == "tasks")
        .unwrap();
    let field =
        |l: &dbine_driver::FormList, k: &str| l.fields.iter().find(|f| f.key == k).unwrap().clone();
    let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // Serverless takes its dependencies from its environment.
    let libs = field(tasks, "libraries");
    assert_eq!(libs.when.as_ref().unwrap().0, "type");
    assert_eq!(
        libs.when_all,
        [(
            "compute".to_string(),
            strs(&[
                "existing",
                "job_cluster",
                "new_cluster",
                "new_cluster_autoscale"
            ])
        )]
    );
    // A cluster's dedicated user, only in a mode that has one.
    let modes = strs(&[
        "",
        "DATA_SECURITY_MODE_AUTO",
        "SINGLE_USER",
        "DATA_SECURITY_MODE_DEDICATED",
        "LEGACY_SINGLE_USER",
        "LEGACY_SINGLE_USER_STANDARD",
    ]);
    let user = field(tasks, "single_user_name");
    assert_eq!(user.when.as_ref().unwrap().0, "compute");
    assert_eq!(
        user.when_all,
        [("data_security_mode".to_string(), modes.clone())]
    );
    let clusters = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .find(|l| l.key == "job_clusters")
        .unwrap();
    let user = field(clusters, "single_user_name");
    assert_eq!(user.when, Some(("data_security_mode".to_string(), modes)));
    assert!(user.when_all.is_empty());
}

#[test]
fn hidden_libraries_and_dedicated_user_are_not_sent() {
    let cur = rich();
    let base = back(&form::form(Some(&cur), &sources(true), None));
    let lib_typo = "esto no es una biblioteca";
    assert!(parse_libraries(lib_typo).is_err());

    // A task on a cluster with libraries moves to serverless: the field
    // hides but keeps its text, typo included; neither is sent nor checked.
    let mut v = base.clone();
    let t = task(&mut v, "ingesta");
    t.insert("compute".into(), json!("serverless"));
    t.insert(
        "libraries".into(),
        json!(format!("pypi:requests==2.32.3\n{lib_typo}")),
    );
    let p = plan::change(&cur, &v).unwrap();
    let (_, _, body) = only(&p);
    let t = &body["new_settings"]["tasks"][0];
    assert_eq!(t["task_key"], "ingesta");
    assert!(t.get("libraries").is_none(), "{t:#}");
    assert!(t.get("job_cluster_key").is_none(), "{t:#}");

    // A job cluster from dedicated to standard: its dedicated user hides
    // and isn't sent.
    let mut v = base.clone();
    let jc = row(&mut v, "job_clusters", "jc", "job_cluster_key");
    assert_eq!(jc["single_user_name"], "ana@example.com");
    jc.insert("data_security_mode".into(), json!("USER_ISOLATION"));
    let p = plan::change(&cur, &v).unwrap();
    let (_, _, body) = only(&p);
    let c = &body["new_settings"]["job_clusters"][0]["new_cluster"];
    assert_eq!(c["data_security_mode"], "USER_ISOLATION");
    assert!(c.get("single_user_name").is_none(), "{c:#}");

    // A new job: a serverless task and a task's new standard cluster, each
    // with the hidden value still in the form.
    let mut v = back(&form::form(None, &sources(true), None));
    v.values
        .insert("name".into(), json!("dbine_test_sched oculto"));
    v.lists.insert(
        "tasks".into(),
        vec![
            serde_json::from_value(json!({
                "task_key": "a", "type": "notebook", "notebook_path": "/x",
                "compute": "serverless", "libraries": lib_typo
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "task_key": "b", "type": "notebook", "notebook_path": "/y",
                "compute": "new_cluster", "spark_version": "15.4.x-scala2.12",
                "node_type_id": "i3.xlarge", "num_workers": 1,
                "data_security_mode": "DATA_SECURITY_MODE_STANDARD",
                "single_user_name": "ana@example.com",
                "libraries": "pypi:pandas==2.2.2"
            }))
            .unwrap(),
        ],
    );
    let p = plan::create(&v).unwrap();
    let (_, _, body) = only(&p);
    let (a, b) = (&body["tasks"][0], &body["tasks"][1]);
    assert!(a.get("libraries").is_none(), "{a:#}");
    assert!(b["new_cluster"].get("single_user_name").is_none(), "{b:#}");
    assert_eq!(
        b["new_cluster"]["data_security_mode"],
        "DATA_SECURITY_MODE_STANDARD"
    );
    // On a cluster the libraries still go.
    assert_eq!(
        b["libraries"],
        json!([{ "pypi": { "package": "pandas==2.2.2" } }])
    );

    // In a dedicated mode, or none chosen, the user still goes.
    for mode in ["", "SINGLE_USER", "DATA_SECURITY_MODE_AUTO"] {
        let mut v = v.clone();
        v.lists.get_mut("tasks").unwrap()[1].insert("data_security_mode".into(), json!(mode));
        let p = plan::create(&v).unwrap();
        let (_, _, body) = only(&p);
        assert_eq!(
            body["tasks"][1]["new_cluster"]["single_user_name"], "ana@example.com",
            "{mode}"
        );
    }
}

// ---------------------------------------------------------------------------
// Warnings: the "⚠" lines the app lists before a script runs
// ---------------------------------------------------------------------------

/// A script's warnings, as the app reads them.
fn warned(p: &Plan) -> Vec<String> {
    p.script()
        .lines()
        .filter_map(|l| l.strip_prefix("-- ⚠ "))
        .map(str::to_string)
        .collect()
}

#[test]
fn a_save_warns_only_about_what_it_risks() {
    let cur = rich();
    let base = back(&form::form(Some(&cur), &sources(true), None));
    // Nothing to save; a new job; one request: nothing to accept.
    assert!(warned(&plan::change(&cur, &base).unwrap()).is_empty());
    assert!(warned(&plan::create(&base).unwrap()).is_empty());
    let mut v = base.clone();
    v.values.insert("description".into(), json!("otra"));
    assert!(warned(&plan::change(&cur, &v).unwrap()).is_empty());
    // A reset keeps the job's id, runs and permissions: no warning.
    let mut v = base.clone();
    v.lists.get_mut("parameters").unwrap().pop();
    let p = plan::change(&cur, &v).unwrap();
    assert_eq!(p.requests[0].path, "/api/2.1/jobs/reset");
    assert!(warned(&p).is_empty(), "{}", p.script());
    // The permissions alone: one request.
    let grant = || -> BTreeMap<String, Json> {
        serde_json::from_value(json!({ "principal_type": "user", "principal": "bea@example.com", "level": "CAN_MANAGE" })).unwrap()
    };
    let mut v = base.clone();
    v.lists.get_mut("permissions").unwrap().push(grant());
    assert!(warned(&plan::change(&cur, &v).unwrap()).is_empty());
    // Settings and permissions: two requests, the second may fail.
    v.values.insert("description".into(), json!("otra"));
    let p = plan::change(&cur, &v).unwrap();
    assert_eq!(p.requests.len(), 2);
    assert_eq!(warned(&p), [plan::NOT_ATOMIC]);
    // A delete takes the runs.
    assert_eq!(warned(&plan::delete(&cur)), [plan::RUNS_GO]);
}

#[test]
fn a_bundles_job_warns_that_its_next_deploy_overwrites_the_change() {
    let mut j = rich_job();
    j["settings"]["deployment"] = json!({ "kind": "BUNDLE", "metadata_file_path": "/x" });
    let cur = Current { job: j, ..rich() };
    let base = back(&form::form(Some(&cur), &sources(true), None));
    let p = plan::change(&cur, &base).unwrap();
    assert!(p.requests.is_empty());
    assert!(warned(&p).is_empty(), "{}", p.script());
    let mut v = base;
    v.values.insert("description".into(), json!("otra"));
    assert_eq!(warned(&plan::change(&cur, &v).unwrap()), [form::BUNDLE]);
}

//! The job's designer: the workspace's job page (Tasks, Schedules &
//! Triggers, Job parameters, Compute, Notifications, Git, Permissions) as a
//! form, and a job's settings as its values. [`super::spec`] reads the
//! values back into settings; an unchanged form gives the job back as it
//! was (the unit tests check it).

use super::lines::{
    render_args, render_libraries, render_list, render_map, render_rule, render_subscriber,
};
use super::spec::{
    task_modeled, task_type, uses_compute, CLUSTER_KEYS, EMAIL_KEYS, HOOK_KEYS, IGNORED, JOB_KEYS,
};
use super::{Current, Sources};
use crate::Json;
use dbine_driver::{FormField, FormKind, FormList, FormSection, ObjectForm};
use serde_json::{json, Map};
use std::collections::BTreeMap;

pub(crate) type Row = BTreeMap<String, Json>;

const BUNDLE: &str = "Este job lo despliega un bundle (Databricks Asset Bundles): lo que se cambie desde DBine se pierde en el próximo deploy del bundle.";
const UI_LOCKED: &str = "Este job está bloqueado en la interfaz del workspace (modo de edición «Bloqueado»): DBine lo cambia por la API igual.";
const MANAGE_ONLY: &str = "Este usuario no tiene CAN_MANAGE sobre el job: puede verlo, pero cambiarlo o borrarlo lo puede solo su propietario, quien tenga CAN_MANAGE o un administrador del workspace.";
const NO_ACL: &str =
    "No se pudieron leer los permisos del job (hace falta CAN_MANAGE): se dejan como están.";
const KEEP_JSON: &str =
    "Lo que no tiene un campo propio, como lo devuelve la API; se guarda tal cual.";
const NO_TRIGGER: &str = "Este job tiene un disparador que DBine no conoce: se conserva como está.";

/// The value at `ptr` as the form shows it: a string or a number as it
/// is, `null` when missing.
fn val(v: &Json, ptr: &str) -> Json {
    match v.pointer(ptr) {
        Some(x @ (Json::String(_) | Json::Number(_))) => x.clone(),
        Some(Json::Bool(b)) => json!(b.to_string()),
        _ => Json::Null,
    }
}

fn flag(v: &Json, ptr: &str) -> Json {
    json!(v.pointer(ptr).and_then(Json::as_bool).unwrap_or(false))
}

fn arr<'a>(v: &'a Json, ptr: &str) -> &'a [Json] {
    v.pointer(ptr)
        .and_then(Json::as_array)
        .map_or(&[][..], Vec::as_slice)
}

fn map_text(v: &Json, ptr: &str) -> Json {
    match v.pointer(ptr) {
        Some(Json::Object(m)) if !m.is_empty() => json!(render_map(m)),
        _ => json!(""),
    }
}

/// The object's keys the form has no field for, as JSON ("" when none).
pub(crate) fn extra_text(v: &Json, modeled: &[&str]) -> String {
    let rest: Map<String, Json> = v
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| !modeled.contains(&k.as_str()) && !IGNORED.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if rest.is_empty() {
        String::new()
    } else {
        serde_json::to_string_pretty(&rest).unwrap_or_default()
    }
}

/// Ids of a notification list (`[{ "id": … }]`): a list to pick from, or
/// one per line.
fn hook_ids(v: &Json, ptr: &str, sources: &Sources) -> Json {
    let ids: Vec<String> = arr(v, ptr)
        .iter()
        .filter_map(|h| h.get("id").and_then(Json::as_str).map(str::to_string))
        .collect();
    if sources.has("destination") {
        json!(ids)
    } else {
        json!(ids.join("\n"))
    }
}

fn put_notifications(r: &mut Row, v: &Json, sources: &Sources) {
    for (api, key) in EMAIL_KEYS {
        r.insert(
            key.into(),
            json!(render_list(arr(v, &format!("/email_notifications/{api}")))),
        );
    }
    for (api, key) in HOOK_KEYS {
        r.insert(
            key.into(),
            hook_ids(v, &format!("/webhook_notifications/{api}"), sources),
        );
    }
    r.insert(
        "no_alert_skipped".into(),
        flag(v, "/notification_settings/no_alert_for_skipped_runs"),
    );
    r.insert(
        "no_alert_canceled".into(),
        flag(v, "/notification_settings/no_alert_for_canceled_runs"),
    );
}

// ---------------------------------------------------------------------------
// Values from the job
// ---------------------------------------------------------------------------

/// A cluster spec's values (a job cluster's row, or a task's own cluster).
pub(crate) fn cluster_values(c: &Json, r: &mut Row) {
    for k in [
        "spark_version",
        "node_type_id",
        "driver_node_type_id",
        "data_security_mode",
        "single_user_name",
        "policy_id",
        "instance_pool_id",
        "driver_instance_pool_id",
        "runtime_engine",
    ] {
        r.insert(k.into(), val(c, &format!("/{k}")));
    }
    r.insert("num_workers".into(), val(c, "/num_workers"));
    r.insert("autoscale_min".into(), val(c, "/autoscale/min_workers"));
    r.insert("autoscale_max".into(), val(c, "/autoscale/max_workers"));
    for k in ["spark_conf", "spark_env_vars", "custom_tags"] {
        r.insert(k.into(), map_text(c, &format!("/{k}")));
    }
    r.insert("cluster_extra".into(), json!(extra_text(c, CLUSTER_KEYS)));
}

/// A task's row: its fields' values and the key it has on the server.
pub(crate) fn task_row(t: &Json, sources: &Sources) -> Row {
    let ty = task_type(t);
    let mut r = Row::new();
    let key = val(t, "/task_key");
    r.insert("orig_key".into(), key.clone());
    r.insert("task_key".into(), key);
    r.insert("type".into(), json!(ty));
    r.insert("description".into(), val(t, "/description"));
    r.insert("disabled".into(), flag(t, "/disabled"));
    let (mut plain, mut yes, mut no) = (Vec::new(), Vec::new(), Vec::new());
    for d in arr(t, "/depends_on") {
        let Some(k) = d.get("task_key").and_then(Json::as_str) else {
            continue;
        };
        match d.get("outcome").and_then(Json::as_str) {
            Some("true") => yes.push(k),
            Some("false") => no.push(k),
            _ => plain.push(k),
        }
    }
    r.insert("depends_on".into(), json!(plain));
    r.insert("depends_true".into(), json!(yes));
    r.insert("depends_false".into(), json!(no));
    r.insert("run_if".into(), val(t, "/run_if"));
    match ty {
        "notebook" => {
            r.insert(
                "notebook_path".into(),
                val(t, "/notebook_task/notebook_path"),
            );
            r.insert("source".into(), val(t, "/notebook_task/source"));
            r.insert(
                "base_parameters".into(),
                map_text(t, "/notebook_task/base_parameters"),
            );
            r.insert("warehouse_id".into(), val(t, "/notebook_task/warehouse_id"));
        }
        "python_script" => {
            r.insert(
                "python_file".into(),
                val(t, "/spark_python_task/python_file"),
            );
            r.insert("source".into(), val(t, "/spark_python_task/source"));
            r.insert(
                "parameters".into(),
                json!(render_args(arr(t, "/spark_python_task/parameters"))),
            );
        }
        "python_wheel" => {
            r.insert(
                "package_name".into(),
                val(t, "/python_wheel_task/package_name"),
            );
            r.insert(
                "entry_point".into(),
                val(t, "/python_wheel_task/entry_point"),
            );
            r.insert(
                "parameters".into(),
                json!(render_args(arr(t, "/python_wheel_task/parameters"))),
            );
            r.insert(
                "named_parameters".into(),
                map_text(t, "/python_wheel_task/named_parameters"),
            );
        }
        "jar" => {
            r.insert(
                "main_class_name".into(),
                val(t, "/spark_jar_task/main_class_name"),
            );
            r.insert(
                "parameters".into(),
                json!(render_args(arr(t, "/spark_jar_task/parameters"))),
            );
        }
        "spark_submit" => {
            r.insert(
                "parameters".into(),
                json!(render_args(arr(t, "/spark_submit_task/parameters"))),
            );
        }
        "sql_query" | "sql_file" | "sql_dashboard" | "sql_alert" => {
            r.insert("warehouse_id".into(), val(t, "/sql_task/warehouse_id"));
            r.insert("sql_parameters".into(), map_text(t, "/sql_task/parameters"));
            match ty {
                "sql_query" => {
                    r.insert("query_id".into(), val(t, "/sql_task/query/query_id"));
                }
                "sql_file" => {
                    r.insert("sql_path".into(), val(t, "/sql_task/file/path"));
                    r.insert("source".into(), val(t, "/sql_task/file/source"));
                }
                "sql_dashboard" => {
                    r.insert(
                        "sql_dashboard_id".into(),
                        val(t, "/sql_task/dashboard/dashboard_id"),
                    );
                    r.insert(
                        "custom_subject".into(),
                        val(t, "/sql_task/dashboard/custom_subject"),
                    );
                    r.insert(
                        "pause_subscriptions".into(),
                        flag(t, "/sql_task/dashboard/pause_subscriptions"),
                    );
                    r.insert(
                        "subscriptions".into(),
                        json!(subscribers(arr(t, "/sql_task/dashboard/subscriptions"))),
                    );
                }
                _ => {
                    r.insert("alert_id".into(), val(t, "/sql_task/alert/alert_id"));
                    r.insert(
                        "pause_subscriptions".into(),
                        flag(t, "/sql_task/alert/pause_subscriptions"),
                    );
                    r.insert(
                        "subscriptions".into(),
                        json!(subscribers(arr(t, "/sql_task/alert/subscriptions"))),
                    );
                }
            }
        }
        "dbt" => {
            r.insert(
                "dbt_commands".into(),
                json!(render_list(arr(t, "/dbt_task/commands"))),
            );
            r.insert(
                "project_directory".into(),
                val(t, "/dbt_task/project_directory"),
            );
            r.insert(
                "profiles_directory".into(),
                val(t, "/dbt_task/profiles_directory"),
            );
            r.insert("dbt_catalog".into(), val(t, "/dbt_task/catalog"));
            r.insert("dbt_schema".into(), val(t, "/dbt_task/schema"));
            r.insert("warehouse_id".into(), val(t, "/dbt_task/warehouse_id"));
            r.insert("source".into(), val(t, "/dbt_task/source"));
        }
        "pipeline" => {
            r.insert("pipeline_id".into(), val(t, "/pipeline_task/pipeline_id"));
            r.insert(
                "full_refresh".into(),
                flag(t, "/pipeline_task/full_refresh"),
            );
        }
        "run_job" => {
            r.insert("run_job_id".into(), val(t, "/run_job_task/job_id"));
            r.insert(
                "job_parameters".into(),
                map_text(t, "/run_job_task/job_parameters"),
            );
        }
        "condition" => {
            r.insert("condition_left".into(), val(t, "/condition_task/left"));
            r.insert("condition_op".into(), val(t, "/condition_task/op"));
            r.insert("condition_right".into(), val(t, "/condition_task/right"));
        }
        "for_each" => {
            r.insert("foreach_inputs".into(), val(t, "/for_each_task/inputs"));
            r.insert(
                "foreach_concurrency".into(),
                val(t, "/for_each_task/concurrency"),
            );
            r.insert(
                "foreach_task".into(),
                json!(t
                    .pointer("/for_each_task/task")
                    .filter(|x| x.is_object())
                    .map(|x| serde_json::to_string_pretty(x).unwrap_or_default())
                    .unwrap_or_default()),
            );
        }
        "dashboard" => {
            r.insert(
                "dashboard_id".into(),
                val(t, "/dashboard_task/dashboard_id"),
            );
            r.insert(
                "warehouse_id".into(),
                val(t, "/dashboard_task/warehouse_id"),
            );
            r.insert(
                "custom_subject".into(),
                val(t, "/dashboard_task/subscription/custom_subject"),
            );
            r.insert(
                "pause_subscriptions".into(),
                flag(t, "/dashboard_task/subscription/paused"),
            );
            r.insert(
                "subscriptions".into(),
                json!(subscribers(arr(
                    t,
                    "/dashboard_task/subscription/subscribers"
                ))),
            );
        }
        _ => {}
    }
    if uses_compute(ty) {
        let compute = if t.get("existing_cluster_id").is_some() {
            "existing"
        } else if t.get("job_cluster_key").is_some() {
            "job_cluster"
        } else if let Some(c) = t.get("new_cluster") {
            cluster_values(c, &mut r);
            if c.get("autoscale").is_some() {
                "new_cluster_autoscale"
            } else {
                "new_cluster"
            }
        } else {
            "serverless"
        };
        r.insert("compute".into(), json!(compute));
        r.insert("existing_cluster_id".into(), val(t, "/existing_cluster_id"));
        r.insert("job_cluster_key".into(), val(t, "/job_cluster_key"));
        r.insert("environment_key".into(), val(t, "/environment_key"));
        r.insert(
            "libraries".into(),
            json!(render_libraries(arr(t, "/libraries"))),
        );
    }
    r.insert("timeout_seconds".into(), val(t, "/timeout_seconds"));
    r.insert("max_retries".into(), val(t, "/max_retries"));
    r.insert(
        "min_retry_interval_millis".into(),
        val(t, "/min_retry_interval_millis"),
    );
    r.insert("retry_on_timeout".into(), flag(t, "/retry_on_timeout"));
    put_notifications(&mut r, t, sources);
    r.insert(
        "alert_on_last_attempt".into(),
        flag(t, "/notification_settings/alert_on_last_attempt"),
    );
    r.insert(
        "health".into(),
        json!(arr(t, "/health/rules")
            .iter()
            .map(render_rule)
            .collect::<Vec<_>>()
            .join("\n")),
    );
    r.insert(
        "disable_auto_optimization".into(),
        flag(t, "/disable_auto_optimization"),
    );
    r.insert("task_extra".into(), json!(extra_text(t, &task_modeled(ty))));
    r
}

fn subscribers(a: &[Json]) -> String {
    a.iter()
        .map(render_subscriber)
        .collect::<Vec<_>>()
        .join("\n")
}

/// How the job starts, as the form says it.
pub(crate) fn trigger_mode(s: &Json) -> &'static str {
    if s.get("schedule").is_some_and(Json::is_object) {
        return "schedule";
    }
    if let Some(t) = s.get("trigger").filter(|t| t.is_object()) {
        return if t.get("periodic").is_some() {
            "periodic"
        } else if t.get("file_arrival").is_some() {
            "file_arrival"
        } else if t.get("table_update").is_some() || t.get("table").is_some() {
            "table_update"
        } else {
            "other"
        };
    }
    if s.get("continuous").is_some_and(Json::is_object) {
        return "continuous";
    }
    "none"
}

/// The job's top-level values.
pub(crate) fn job_values(s: &Json, sources: &Sources) -> Row {
    let mut v = Row::new();
    v.insert("name".into(), val(s, "/name"));
    v.insert("description".into(), val(s, "/description"));
    v.insert("tags".into(), map_text(s, "/tags"));
    let (mode, who) = if let Some(u) = s.pointer("/run_as/user_name") {
        ("user", u.clone())
    } else if let Some(sp) = s.pointer("/run_as/service_principal_name") {
        ("service_principal", sp.clone())
    } else {
        ("owner", Json::Null)
    };
    v.insert("run_as_mode".into(), json!(mode));
    v.insert("run_as_name".into(), who);
    v.insert("edit_mode".into(), val(s, "/edit_mode"));
    v.insert("performance_target".into(), val(s, "/performance_target"));
    v.insert("budget_policy_id".into(), val(s, "/budget_policy_id"));

    let mode = trigger_mode(s);
    v.insert("trigger_mode".into(), json!(mode));
    let paused_at = match mode {
        "schedule" => "/schedule/pause_status",
        "continuous" => "/continuous/pause_status",
        _ => "/trigger/pause_status",
    };
    v.insert(
        "paused".into(),
        json!(s.pointer(paused_at).and_then(Json::as_str) == Some("PAUSED")),
    );
    v.insert("cron".into(), val(s, "/schedule/quartz_cron_expression"));
    v.insert("timezone".into(), val(s, "/schedule/timezone_id"));
    v.insert(
        "periodic_interval".into(),
        val(s, "/trigger/periodic/interval"),
    );
    v.insert("periodic_unit".into(), val(s, "/trigger/periodic/unit"));
    v.insert("file_url".into(), val(s, "/trigger/file_arrival/url"));
    let table = if s.pointer("/trigger/table").is_some() {
        "/trigger/table"
    } else {
        "/trigger/table_update"
    };
    let timing = match mode {
        "file_arrival" => "/trigger/file_arrival",
        _ => table,
    };
    v.insert(
        "table_names".into(),
        json!(render_list(arr(s, &format!("{table}/table_names")))),
    );
    v.insert(
        "table_condition".into(),
        val(s, &format!("{table}/condition")),
    );
    v.insert(
        "min_between".into(),
        val(s, &format!("{timing}/min_time_between_triggers_seconds")),
    );
    v.insert(
        "wait_after".into(),
        val(s, &format!("{timing}/wait_after_last_change_seconds")),
    );
    v.insert(
        "continuous_retry".into(),
        val(s, "/continuous/task_retry_mode"),
    );
    v.insert("max_concurrent_runs".into(), val(s, "/max_concurrent_runs"));
    v.insert("queue".into(), flag(s, "/queue/enabled"));
    v.insert("timeout_seconds".into(), val(s, "/timeout_seconds"));
    put_notifications(&mut v, s, sources);
    v.insert("git_url".into(), val(s, "/git_source/git_url"));
    v.insert("git_provider".into(), val(s, "/git_source/git_provider"));
    let (kind, r) = ["git_branch", "git_tag", "git_commit"]
        .iter()
        .find_map(|k| {
            s.pointer(&format!("/git_source/{k}"))
                .map(|r| (k.trim_start_matches("git_"), r.clone()))
        })
        .unwrap_or(("branch", Json::Null));
    v.insert("git_ref_kind".into(), json!(kind));
    v.insert("git_ref".into(), r);
    v.insert("settings_extra".into(), json!(extra_text(s, JOB_KEYS)));
    v
}

// ---------------------------------------------------------------------------
// The form's fields
// ---------------------------------------------------------------------------

/// A cluster spec's fields. `mode`: the field that says fixed size or
/// autoscaling, and its values for each (and for both: shown for either).
fn cluster_fields(
    sources: &Sources,
    mode: &str,
    fixed: &[&str],
    auto: &[&str],
    any: Option<&[&str]>,
) -> Vec<FormField> {
    let all = |f: FormField| match any {
        Some(vals) => f.when(mode, vals),
        None => f,
    };
    vec![
        all(FormField::new(
            "spark_version",
            "Versión de Databricks Runtime",
            sources.one("spark_version"),
        )
        .placeholder("15.4.x-scala2.12")),
        all(FormField::new("node_type_id", "Tipo de nodo de los workers", sources.one("node_type"))
            .placeholder("i3.xlarge")),
        all(FormField::new("driver_node_type_id", "Tipo de nodo del driver", sources.one("node_type"))
            .help("Vacío: el mismo que los workers.")),
        FormField::new("num_workers", "Workers", FormKind::number(Some(0.0), Some(100_000.0)))
            .when(mode, fixed)
            .help("0: un clúster de un solo nodo."),
        FormField::new("autoscale_min", "Mínimo de workers", FormKind::number(Some(0.0), Some(100_000.0)))
            .when(mode, auto),
        FormField::new("autoscale_max", "Máximo de workers", FormKind::number(Some(1.0), Some(100_000.0)))
            .when(mode, auto),
        all(FormField::new(
            "data_security_mode",
            "Modo de acceso",
            FormKind::select(&[
                ("DATA_SECURITY_MODE_AUTO", "Automático"),
                ("SINGLE_USER", "Dedicado (un usuario)"),
                ("USER_ISOLATION", "Estándar (compartido)"),
                ("DATA_SECURITY_MODE_DEDICATED", "Dedicado"),
                ("DATA_SECURITY_MODE_STANDARD", "Estándar"),
                ("NONE", "Sin aislamiento"),
                ("LEGACY_TABLE_ACL", "Heredado: control de acceso a tablas"),
                ("LEGACY_PASSTHROUGH", "Heredado: transferencia de credenciales"),
                ("LEGACY_SINGLE_USER", "Heredado: un usuario"),
                ("LEGACY_SINGLE_USER_STANDARD", "Heredado: un usuario estándar"),
            ]),
        )),
        all(FormField::new("single_user_name", "Usuario dedicado", FormKind::Text)
            .help("El usuario o grupo que usa el clúster en modo dedicado.")),
        all(FormField::new("policy_id", "Política de clúster", sources.one("policy"))),
        all(FormField::new("instance_pool_id", "Pool de instancias", sources.one("instance_pool"))),
        all(FormField::new(
            "driver_instance_pool_id",
            "Pool de instancias del driver",
            sources.one("instance_pool"),
        )),
        all(FormField::new(
            "runtime_engine",
            "Motor",
            FormKind::select(&[("STANDARD", "Estándar"), ("PHOTON", "Photon")]),
        )),
        all(FormField::new("spark_conf", "Configuración de Spark", FormKind::Textarea)
            .help("Una por línea: clave=valor.")
            .placeholder("spark.sql.shuffle.partitions=200")),
        all(FormField::new("spark_env_vars", "Variables de entorno", FormKind::Textarea)
            .help("Una por línea: NOMBRE=valor.")),
        all(FormField::new("custom_tags", "Etiquetas del clúster", FormKind::Textarea)
            .help("Una por línea: clave=valor.")),
        all(FormField::new("cluster_extra", "Otros atributos del clúster (JSON)", FormKind::code("json"))
            .help("Atributos de la nube, scripts de inicio, logs, Docker… como los toma la API (un objeto JSON).")),
    ]
}

const COMPUTE_TYPES: [&str; 6] = [
    "notebook",
    "python_script",
    "python_wheel",
    "jar",
    "spark_submit",
    "dbt",
];
const NEW_CLUSTER: [&str; 2] = ["new_cluster", "new_cluster_autoscale"];

/// The task editor: the workspace's task panel.
pub(crate) fn task_fields(sources: &Sources) -> Vec<FormField> {
    let sql = ["sql_query", "sql_file", "sql_dashboard", "sql_alert"];
    let mut f = vec![
        FormField::new("task_key", "Nombre de la tarea", FormKind::Text)
            .required()
            .help("Letras, números, «_» y «-»; único en el job."),
        FormField::new(
            "type",
            "Tipo",
            FormKind::select(&[
                ("notebook", "Notebook"),
                ("python_script", "Script de Python"),
                ("python_wheel", "Python wheel"),
                ("jar", "JAR"),
                ("spark_submit", "Spark Submit"),
                ("sql_query", "SQL: consulta"),
                ("sql_file", "SQL: archivo"),
                ("sql_dashboard", "SQL: dashboard heredado"),
                ("sql_alert", "SQL: alerta"),
                ("dashboard", "Dashboard"),
                ("dbt", "dbt"),
                ("pipeline", "Pipeline"),
                ("run_job", "Ejecutar otro job"),
                ("condition", "Condición (if/else)"),
                ("for_each", "Para cada elemento"),
                ("other", "Otro (JSON)"),
            ]),
        )
        .required()
        .default(json!("notebook"))
        .help("«Otro»: el tipo va en «Otros atributos (JSON)», como lo toma la API (Power BI, Clean Rooms…)."),
        FormField::new("description", "Descripción", FormKind::Textarea),
        FormField::new("disabled", "Deshabilitada", FormKind::Bool)
            .help("La tarea no se ejecuta aunque sea parte del job."),
        FormField::new("depends_on", "Depende de", FormKind::reference("list:tasks", true)),
        FormField::new(
            "depends_true",
            "Depende de la condición, si es verdadera",
            FormKind::reference("list:tasks", true),
        )
        .help("Tareas de condición: esta tarea se ejecuta por su rama verdadera."),
        FormField::new(
            "depends_false",
            "Depende de la condición, si es falsa",
            FormKind::reference("list:tasks", true),
        )
        .help("Tareas de condición: esta tarea se ejecuta por su rama falsa."),
        FormField::new(
            "run_if",
            "Ejecutar si",
            FormKind::select(&[
                ("ALL_SUCCESS", "Todas las dependencias terminaron bien"),
                ("AT_LEAST_ONE_SUCCESS", "Al menos una terminó bien"),
                ("NONE_FAILED", "Ninguna falló"),
                ("ALL_DONE", "Todas terminaron"),
                ("AT_LEAST_ONE_FAILED", "Al menos una falló"),
                ("ALL_FAILED", "Todas fallaron"),
            ]),
        )
        .default(json!("ALL_SUCCESS")),
        // Notebook.
        FormField::new("notebook_path", "Notebook", FormKind::Text)
            .when("type", &["notebook"])
            .placeholder("/Workspace/Users/ana@example.com/carga"),
        // Python script, wheel, JAR.
        FormField::new("python_file", "Archivo de Python", FormKind::Text)
            .when("type", &["python_script"])
            .placeholder("/Workspace/etl/carga.py"),
        FormField::new("package_name", "Paquete", FormKind::Text).when("type", &["python_wheel"]),
        FormField::new("entry_point", "Punto de entrada", FormKind::Text)
            .when("type", &["python_wheel"]),
        FormField::new("main_class_name", "Clase principal", FormKind::Text)
            .when("type", &["jar"])
            .placeholder("com.example.Carga"),
        FormField::new("parameters", "Argumentos", FormKind::Textarea)
            .when("type", &["python_script", "python_wheel", "jar", "spark_submit"])
            .help("Uno por línea (o una lista JSON)."),
        FormField::new("named_parameters", "Argumentos con nombre", FormKind::Textarea)
            .when("type", &["python_wheel"])
            .help("Uno por línea: nombre=valor."),
        FormField::new(
            "source",
            "Origen",
            FormKind::select(&[("WORKSPACE", "Workspace"), ("GIT", "Repositorio Git del job")]),
        )
        .when("type", &["notebook", "python_script", "sql_file", "dbt"])
        .help("Vacío: el repositorio Git si el job tiene uno, si no el workspace."),
        FormField::new("base_parameters", "Parámetros", FormKind::Textarea)
            .when("type", &["notebook"])
            .help("Uno por línea: nombre=valor."),
        // SQL and dashboards.
        FormField::new("warehouse_id", "SQL warehouse", sources.one("warehouse"))
            .when("type", &["notebook", "sql_query", "sql_file", "sql_dashboard", "sql_alert", "dashboard", "dbt"])
            .help("En un notebook, para ejecutarlo en un SQL warehouse en lugar de un clúster."),
        FormField::new("query_id", "Id de la consulta", FormKind::Text).when("type", &["sql_query"]),
        FormField::new("sql_path", "Archivo SQL", FormKind::Text)
            .when("type", &["sql_file"])
            .placeholder("/Workspace/sql/modelo.sql"),
        FormField::new("sql_dashboard_id", "Id del dashboard", FormKind::Text)
            .when("type", &["sql_dashboard"]),
        FormField::new("alert_id", "Id de la alerta", FormKind::Text).when("type", &["sql_alert"]),
        FormField::new("dashboard_id", "Id del dashboard", FormKind::Text).when("type", &["dashboard"]),
        FormField::new("sql_parameters", "Parámetros", FormKind::Textarea)
            .when("type", &sql)
            .help("Uno por línea: nombre=valor."),
        FormField::new("custom_subject", "Asunto del correo", FormKind::Text)
            .when("type", &["sql_dashboard", "dashboard"]),
        FormField::new("pause_subscriptions", "Suscripciones en pausa", FormKind::Bool)
            .when("type", &["sql_dashboard", "sql_alert", "dashboard"]),
        FormField::new("subscriptions", "Suscriptores", FormKind::Textarea)
            .when("type", &["sql_dashboard", "sql_alert", "dashboard"])
            .help("Uno por línea: el usuario, o destination:<id> para un destino de notificación."),
        // dbt.
        FormField::new("dbt_commands", "Comandos de dbt", FormKind::Textarea)
            .when("type", &["dbt"])
            .help("Uno por línea; cada uno empieza con «dbt».")
            .placeholder("dbt deps\ndbt run"),
        FormField::new("project_directory", "Carpeta del proyecto", FormKind::Text).when("type", &["dbt"]),
        FormField::new("profiles_directory", "Carpeta de perfiles", FormKind::Text).when("type", &["dbt"]),
        FormField::new("dbt_catalog", "Catálogo", FormKind::Text).when("type", &["dbt"]),
        FormField::new("dbt_schema", "Esquema", FormKind::Text).when("type", &["dbt"]),
        // Pipeline, another job, condition, for each.
        FormField::new("pipeline_id", "Pipeline", sources.one("pipeline")).when("type", &["pipeline"]),
        FormField::new("full_refresh", "Actualización completa", FormKind::Bool).when("type", &["pipeline"]),
        FormField::new("run_job_id", "Job", sources.one("job")).when("type", &["run_job"]),
        FormField::new("job_parameters", "Parámetros del job", FormKind::Textarea)
            .when("type", &["run_job"])
            .help("Uno por línea: nombre=valor."),
        FormField::new("condition_left", "Valor izquierdo", FormKind::Text)
            .when("type", &["condition"])
            .placeholder("{{tasks.carga.values.filas}}"),
        FormField::new(
            "condition_op",
            "Operador",
            FormKind::select(&[
                ("EQUAL_TO", "igual a"),
                ("NOT_EQUAL", "distinto de"),
                ("GREATER_THAN", "mayor que"),
                ("GREATER_THAN_OR_EQUAL", "mayor o igual que"),
                ("LESS_THAN", "menor que"),
                ("LESS_THAN_OR_EQUAL", "menor o igual que"),
            ]),
        )
        .when("type", &["condition"])
        .default(json!("EQUAL_TO")),
        FormField::new("condition_right", "Valor derecho", FormKind::Text).when("type", &["condition"]),
        FormField::new("foreach_inputs", "Elementos", FormKind::Textarea)
            .when("type", &["for_each"])
            .help("Una lista JSON o una referencia a un valor ({{job.parameters.lista}}).")
            .placeholder("[\"a\", \"b\"]"),
        FormField::new(
            "foreach_concurrency",
            "Iteraciones a la vez",
            FormKind::number(Some(1.0), Some(100.0)),
        )
        .when("type", &["for_each"]),
        FormField::new("foreach_task", "Tarea de cada iteración (JSON)", FormKind::code("json"))
            .when("type", &["for_each"])
            .help("La tarea que se repite, como la toma la API (task_key y su tipo)."),
        // Compute.
        FormField::new(
            "compute",
            "Cómputo",
            FormKind::select(&[
                ("serverless", "Serverless"),
                ("existing", "Clúster existente"),
                ("job_cluster", "Clúster del job"),
                ("new_cluster", "Clúster nuevo, de tamaño fijo"),
                ("new_cluster_autoscale", "Clúster nuevo, con escalado automático"),
            ]),
        )
        .when("type", &COMPUTE_TYPES)
        .default(json!("serverless")),
        FormField::new("existing_cluster_id", "Clúster", sources.one("cluster"))
            .when("compute", &["existing"]),
        FormField::new("job_cluster_key", "Clúster del job", FormKind::reference("list:job_clusters", false))
            .when("compute", &["job_cluster"]),
        FormField::new("environment_key", "Entorno", FormKind::reference("list:environments", false))
            .when("compute", &["serverless"])
            .help("Scripts de Python, wheels y dbt en serverless: el entorno con sus dependencias."),
    ];
    f.extend(cluster_fields(
        sources,
        "compute",
        &["new_cluster"],
        &["new_cluster_autoscale"],
        Some(&NEW_CLUSTER),
    ));
    f.extend([
        FormField::new("libraries", "Bibliotecas", FormKind::Textarea)
            .when("type", &COMPUTE_TYPES)
            .help("Una por línea: pypi:paquete==versión [repo=…], maven:grupo:artefacto:versión [repo=…] [exclusions=…], cran:paquete, jar:ruta, whl:ruta, egg:ruta, requirements:ruta.")
            .placeholder("pypi:pandas==2.2.2"),
        // Retries and limits.
        FormField::new("timeout_seconds", "Tiempo máximo", FormKind::Duration)
            .help("0 o vacío: sin límite."),
        FormField::new("max_retries", "Reintentos", FormKind::number(Some(-1.0), Some(1_000_000.0)))
            .help("-1: sin límite."),
        FormField::new(
            "min_retry_interval_millis",
            "Espera entre reintentos (ms)",
            FormKind::number(Some(0.0), None),
        ),
        FormField::new("retry_on_timeout", "Reintentar si se agota el tiempo", FormKind::Bool),
        FormField::new("health", "Reglas de salud", FormKind::Textarea)
            .help("Una por línea: MÉTRICA > valor (RUN_DURATION_SECONDS, STREAMING_BACKLOG_BYTES, STREAMING_BACKLOG_RECORDS, STREAMING_BACKLOG_SECONDS, STREAMING_BACKLOG_FILES).")
            .placeholder("RUN_DURATION_SECONDS > 3600"),
        FormField::new("disable_auto_optimization", "Sin optimización automática", FormKind::Bool),
    ]);
    f.extend(notification_fields(sources));
    f.push(FormField::new(
        "alert_on_last_attempt",
        "Avisar solo en el último reintento",
        FormKind::Bool,
    ));
    f.push(
        FormField::new(
            "task_extra",
            "Otros atributos (JSON)",
            FormKind::code("json"),
        )
        .help(KEEP_JSON),
    );
    f
}

/// Mail and destinations for each event, and what not to tell.
fn notification_fields(sources: &Sources) -> Vec<FormField> {
    let mails = "Direcciones, una por línea.";
    let hooks = if sources.has("destination") {
        "Destinos de notificación del workspace."
    } else {
        "Ids de destinos de notificación, uno por línea."
    };
    vec![
        FormField::new("email_on_start", "Correo al empezar", FormKind::Textarea).help(mails),
        FormField::new(
            "email_on_success",
            "Correo al terminar bien",
            FormKind::Textarea,
        )
        .help(mails),
        FormField::new("email_on_failure", "Correo al fallar", FormKind::Textarea).help(mails),
        FormField::new(
            "email_on_duration_warning",
            "Correo si supera la duración esperada",
            FormKind::Textarea,
        )
        .help(mails),
        FormField::new(
            "email_on_streaming_backlog",
            "Correo si se acumula backlog de streaming",
            FormKind::Textarea,
        )
        .help(mails),
        FormField::new(
            "hook_on_start",
            "Avisar al empezar",
            sources.many("destination"),
        )
        .help(hooks),
        FormField::new(
            "hook_on_success",
            "Avisar al terminar bien",
            sources.many("destination"),
        )
        .help(hooks),
        FormField::new(
            "hook_on_failure",
            "Avisar al fallar",
            sources.many("destination"),
        )
        .help(hooks),
        FormField::new(
            "hook_on_duration_warning",
            "Avisar si supera la duración esperada",
            sources.many("destination"),
        )
        .help(hooks),
        FormField::new(
            "hook_on_streaming_backlog",
            "Avisar si se acumula backlog de streaming",
            sources.many("destination"),
        )
        .help(hooks),
        FormField::new(
            "no_alert_skipped",
            "No avisar de las ejecuciones omitidas",
            FormKind::Bool,
        ),
        FormField::new(
            "no_alert_canceled",
            "No avisar de las ejecuciones canceladas",
            FormKind::Bool,
        ),
    ]
}

fn permission_level() -> FormKind {
    FormKind::select(&[
        ("IS_OWNER", "Propietario"),
        ("CAN_MANAGE", "Puede administrar"),
        ("CAN_MANAGE_RUN", "Puede administrar las ejecuciones"),
        ("CAN_VIEW", "Puede ver"),
    ])
}

/// The principal of an ACL entry: its kind and name.
pub(crate) fn principal(e: &Json) -> Option<(&'static str, String)> {
    [
        ("user", "user_name"),
        ("group", "group_name"),
        ("service_principal", "service_principal_name"),
    ]
    .iter()
    .find_map(|(kind, k)| {
        e.get(*k)
            .and_then(Json::as_str)
            .map(|n| (*kind, n.to_string()))
    })
}

/// The ACL's rows: the direct permissions (editable) and the inherited
/// ones (shown).
pub(crate) fn acl_rows(acl: &[Json]) -> (Vec<Row>, Vec<Row>) {
    let (mut direct, mut inherited) = (Vec::new(), Vec::new());
    for e in acl {
        let Some((kind, name)) = principal(e) else {
            continue;
        };
        for p in arr(e, "/all_permissions") {
            let Some(level) = p.get("permission_level").and_then(Json::as_str) else {
                continue;
            };
            if p.get("inherited").and_then(Json::as_bool) == Some(true) {
                let from = arr(p, "/inherited_from_object")
                    .iter()
                    .filter_map(Json::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                inherited.push(Row::from([
                    ("principal_type".into(), json!(kind)),
                    ("principal".into(), json!(name)),
                    ("level".into(), json!(level)),
                    ("from".into(), json!(from)),
                ]));
            } else {
                direct.push(Row::from([
                    ("principal_type".into(), json!(kind)),
                    ("principal".into(), json!(name)),
                    ("level".into(), json!(level)),
                ]));
            }
        }
    }
    (direct, inherited)
}

/// Whether the login may change the job: an admin, or CAN_MANAGE /
/// IS_OWNER for it or a group of its. `None`: unknown. SCIM `Me` lists
/// only the groups the login is directly in, so a grant to another group
/// (which may hold the login's group) leaves it unknown: the API decides.
fn may_manage(acl: Option<&[Json]>, me: Option<&Json>) -> Option<bool> {
    let me = me?;
    if crate::permissions::is_admin(me) {
        return Some(true);
    }
    let acl = acl?;
    let user = me
        .get("userName")
        .and_then(Json::as_str)
        .unwrap_or_default();
    let groups: Vec<&str> = arr(me, "/groups")
        .iter()
        .filter_map(|g| g.get("display").and_then(Json::as_str))
        .collect();
    let manages = |e: &Json| {
        arr(e, "/all_permissions").iter().any(|p| {
            matches!(
                p.get("permission_level").and_then(Json::as_str),
                Some("CAN_MANAGE" | "IS_OWNER")
            )
        })
    };
    let mut unknown = false;
    for e in acl.iter().filter(|e| manages(e)) {
        match principal(e) {
            Some(("user" | "service_principal", n)) if n.eq_ignore_ascii_case(user) => {
                return Some(true)
            }
            Some(("group", n)) if n == "users" || groups.contains(&n.as_str()) => {
                return Some(true)
            }
            // The workspace's admins are in `admins` directly (checked above).
            Some(("group", n)) if n != "admins" => unknown = true,
            _ => {}
        }
    }
    (!unknown).then_some(false)
}

/// The designer's form: a new job's defaults (`cur` `None`) or the job as
/// the workspace has it.
pub(crate) fn form(cur: Option<&Current>, sources: &Sources, me: Option<&Json>) -> ObjectForm {
    let settings = cur.map_or_else(|| Json::Object(Map::new()), Current::effective);
    let s = &settings;
    let mut values = job_values(s, sources);
    let mut notes = Vec::new();
    let mut task_rows = Vec::new();
    let mut cluster_rows = Vec::new();
    let mut env_rows = Vec::new();
    let mut param_rows = Vec::new();
    let mut health_rows = Vec::new();
    let (mut acl_direct, mut acl_inherited) = (Vec::new(), Vec::new());
    match cur {
        None => {
            values.insert("name".into(), json!(""));
            values.insert("run_as_mode".into(), json!("owner"));
            values.insert("trigger_mode".into(), json!("none"));
            values.insert("paused".into(), json!(false));
            values.insert("timezone".into(), json!("UTC"));
            values.insert("periodic_unit".into(), json!("DAYS"));
            values.insert("table_condition".into(), json!("ANY_UPDATED"));
            values.insert("max_concurrent_runs".into(), json!(1));
            values.insert("queue".into(), json!(true));
            values.insert("git_ref_kind".into(), json!("branch"));
            values.insert("permissions_read".into(), json!(true));
        }
        Some(c) => {
            for t in arr(s, "/tasks") {
                task_rows.push(task_row(t, sources));
            }
            for jc in arr(s, "/job_clusters") {
                let mut r = Row::new();
                let key = val(jc, "/job_cluster_key");
                r.insert("orig_key".into(), key.clone());
                r.insert("job_cluster_key".into(), key);
                let spec = jc.get("new_cluster").unwrap_or(&Json::Null);
                r.insert(
                    "workers".into(),
                    json!(if spec.get("autoscale").is_some() {
                        "autoscale"
                    } else {
                        "fixed"
                    }),
                );
                cluster_values(spec, &mut r);
                cluster_rows.push(r);
            }
            for e in arr(s, "/environments") {
                let spec = e.get("spec").unwrap_or(&Json::Null);
                env_rows.push(Row::from([
                    ("orig_key".into(), val(e, "/environment_key")),
                    ("environment_key".into(), val(e, "/environment_key")),
                    (
                        "environment_version".into(),
                        val(spec, "/environment_version"),
                    ),
                    ("client".into(), val(spec, "/client")),
                    (
                        "dependencies".into(),
                        json!(render_list(arr(spec, "/dependencies"))),
                    ),
                    (
                        "env_extra".into(),
                        json!(extra_text(
                            spec,
                            &["environment_version", "client", "dependencies"]
                        )),
                    ),
                ]));
            }
            for p in arr(s, "/parameters") {
                param_rows.push(Row::from([
                    ("name".into(), val(p, "/name")),
                    ("default".into(), val(p, "/default")),
                ]));
            }
            for r in arr(s, "/health/rules") {
                let mut row = Row::from([
                    ("metric".into(), val(r, "/metric")),
                    ("value".into(), val(r, "/value")),
                ]);
                if let Some(op) = r.get("op").filter(|o| o.as_str() != Some("GREATER_THAN")) {
                    row.insert("op".into(), op.clone());
                }
                health_rows.push(row);
            }
            values.insert("job_id".into(), json!(c.id.to_string()));
            values.insert("creator".into(), val(&c.job, "/creator_user_name"));
            values.insert(
                "created".into(),
                json!(c
                    .job
                    .get("created_time")
                    .and_then(Json::as_i64)
                    .and_then(crate::jobs::rfc3339)
                    .unwrap_or_default()),
            );
            values.insert("run_as_user".into(), val(&c.job, "/run_as_user_name"));
            values.insert("version".into(), json!(super::plan::version(c)));
            values.insert("permissions_read".into(), json!(c.acl.is_some()));
            match &c.acl {
                Some(acl) => (acl_direct, acl_inherited) = acl_rows(acl),
                None => notes.push(NO_ACL.to_string()),
            }
            if s.pointer("/deployment/kind").and_then(Json::as_str) == Some("BUNDLE") {
                notes.push(BUNDLE.to_string());
            } else if s.get("edit_mode").and_then(Json::as_str) == Some("UI_LOCKED") {
                notes.push(UI_LOCKED.to_string());
            }
            if trigger_mode(s) == "other" {
                notes.push(NO_TRIGGER.to_string());
            }
        }
    }

    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("description", "Descripción", FormKind::Textarea),
        FormField::new("tags", "Etiquetas", FormKind::Textarea)
            .help("Una por línea: clave=valor (o solo la clave). Hasta 25."),
        FormField::new(
            "run_as_mode",
            "Ejecutar como",
            FormKind::select(&[
                ("owner", "El propietario del job"),
                ("user", "Un usuario"),
                ("service_principal", "Un service principal"),
            ]),
        )
        .default(json!("owner"))
        .help("Otro usuario solo lo elige un administrador; un service principal, quien tenga su rol de usuario."),
        FormField::new("run_as_name", "Usuario o service principal", FormKind::Text)
            .when("run_as_mode", &["user", "service_principal"])
            .help("El correo del usuario, o el application id del service principal."),
        FormField::new(
            "performance_target",
            "Rendimiento (serverless)",
            FormKind::select(&[
                ("STANDARD", "Estándar"),
                ("PERFORMANCE_OPTIMIZED", "Optimizado para el rendimiento"),
            ]),
        ),
        FormField::new("budget_policy_id", "Política de presupuesto", FormKind::Text)
            .help("El id de la política de presupuesto de serverless que se atribuye al job."),
        FormField::new(
            "edit_mode",
            "Modo de edición",
            FormKind::select(&[
                ("EDITABLE", "Editable"),
                ("UI_LOCKED", "Bloqueado en la interfaz del workspace"),
            ]),
        ),
    ]);
    if cur.is_some() {
        general = general.fields(vec![
            FormField::new("job_id", "Id del job", FormKind::Text).read_only(true),
            FormField::new("creator", "Creador", FormKind::Text).read_only(true),
            FormField::new("created", "Creado", FormKind::Text).read_only(true),
            FormField::new("run_as_user", "Se ejecuta como", FormKind::Text).read_only(true),
        ]);
    }

    let tasks = FormSection::new("tasks", "Tareas")
        .list(FormList::new("tasks", "Tareas", "task_key", task_fields(sources)).rows(task_rows));

    let schedule = FormSection::new("schedule", "Programación")
        .fields(vec![
            FormField::new(
                "trigger_mode",
                "Cómo empieza",
                FormKind::select(&[
                    ("none", "Solo a mano o desde otro job"),
                    ("schedule", "Programación (cron)"),
                    ("periodic", "Cada cierto tiempo"),
                    ("file_arrival", "Al llegar archivos"),
                    ("table_update", "Al actualizarse tablas"),
                    ("continuous", "Continuo"),
                    ("other", "Otro disparador (se conserva)"),
                ]),
            )
            .required()
            .default(json!("none")),
            FormField::new("paused", "En pausa", FormKind::Bool).when(
                "trigger_mode",
                &["schedule", "periodic", "file_arrival", "table_update", "continuous", "other"],
            ),
            FormField::new("cron", "Expresión cron (Quartz)", FormKind::Cron)
                .when("trigger_mode", &["schedule"])
                .placeholder("0 0 3 * * ?")
                .help("Segundos, minutos, horas, día del mes, mes, día de la semana y, opcional, año."),
            FormField::new("timezone", "Zona horaria", FormKind::Text)
                .when("trigger_mode", &["schedule"])
                .default(json!("UTC"))
                .placeholder("America/Argentina/Buenos_Aires"),
            FormField::new(
                "periodic_interval",
                "Cada",
                FormKind::number(Some(1.0), Some(100_000.0)),
            )
            .when("trigger_mode", &["periodic"]),
            FormField::new(
                "periodic_unit",
                "Unidad",
                FormKind::select(&[("HOURS", "Horas"), ("DAYS", "Días"), ("WEEKS", "Semanas")]),
            )
            .when("trigger_mode", &["periodic"])
            .default(json!("DAYS")),
            FormField::new("file_url", "Ubicación", FormKind::Text)
                .when("trigger_mode", &["file_arrival"])
                .placeholder("s3://bucket/entrada/")
                .help("Una ubicación externa o un volumen de Unity Catalog."),
            FormField::new("table_names", "Tablas", FormKind::Textarea)
                .when("trigger_mode", &["table_update"])
                .help("Una por línea: catálogo.esquema.tabla."),
            FormField::new(
                "table_condition",
                "Cuándo",
                FormKind::select(&[
                    ("ANY_UPDATED", "Al actualizarse cualquiera"),
                    ("ALL_UPDATED", "Al actualizarse todas"),
                ]),
            )
            .when("trigger_mode", &["table_update"])
            .default(json!("ANY_UPDATED")),
            FormField::new("min_between", "Tiempo mínimo entre disparos", FormKind::Duration)
                .when("trigger_mode", &["file_arrival", "table_update"])
                .help("Vacío o 0: sin mínimo; si no, al menos 60 segundos."),
            FormField::new("wait_after", "Esperar tras el último cambio", FormKind::Duration)
                .when("trigger_mode", &["file_arrival", "table_update"])
                .help("Vacío o 0: sin espera; si no, al menos 60 segundos."),
            FormField::new(
                "continuous_retry",
                "Reintentos de las tareas",
                FormKind::select(&[
                    ("ON_FAILURE", "Reintentar las tareas que fallan"),
                    ("NEVER", "No reintentar"),
                ]),
            )
            .when("trigger_mode", &["continuous"]),
            FormField::new(
                "max_concurrent_runs",
                "Máximo de ejecuciones a la vez",
                FormKind::number(Some(0.0), Some(1000.0)),
            )
            .default(json!(1))
            .help("0: se omiten todas las ejecuciones nuevas."),
            FormField::new("queue", "Encolar las ejecuciones que no pueden empezar", FormKind::Bool)
                .default(json!(true)),
            FormField::new("timeout_seconds", "Tiempo máximo de cada ejecución", FormKind::Duration)
                .help("0 o vacío: sin límite."),
        ])
        .list(
            FormList::new(
                "health",
                "Reglas de salud",
                "metric",
                vec![
                    FormField::new(
                        "metric",
                        "Métrica",
                        FormKind::select(&[
                            ("RUN_DURATION_SECONDS", "Duración de la ejecución (segundos)"),
                            ("STREAMING_BACKLOG_BYTES", "Backlog de streaming (bytes)"),
                            ("STREAMING_BACKLOG_RECORDS", "Backlog de streaming (registros)"),
                            ("STREAMING_BACKLOG_SECONDS", "Backlog de streaming (segundos)"),
                            ("STREAMING_BACKLOG_FILES", "Backlog de streaming (archivos)"),
                        ]),
                    )
                    .required()
                    .default(json!("RUN_DURATION_SECONDS")),
                    FormField::new("value", "Mayor que", FormKind::number(Some(0.0), None)).required(),
                ],
            )
            .rows(health_rows),
        );

    let parameters = FormSection::new("parameters", "Parámetros").list(
        FormList::new(
            "parameters",
            "Parámetros del job",
            "name",
            vec![
                FormField::new("name", "Nombre", FormKind::Text)
                    .required()
                    .help("Letras, números, «_», «-» y «.»."),
                FormField::new("default", "Valor predeterminado", FormKind::Text),
            ],
        )
        .rows(param_rows),
    );

    let mut cluster_list = vec![
        FormField::new("job_cluster_key", "Nombre del clúster", FormKind::Text)
            .required()
            .help("Las tareas lo eligen por este nombre."),
        FormField::new(
            "workers",
            "Tamaño",
            FormKind::select(&[("fixed", "Fijo"), ("autoscale", "Escalado automático")]),
        )
        .default(json!("fixed")),
    ];
    cluster_list.extend(cluster_fields(
        sources,
        "workers",
        &["fixed"],
        &["autoscale"],
        None,
    ));
    let compute = FormSection::new("compute", "Cómputo")
        .list(
            FormList::new(
                "job_clusters",
                "Clústeres del job",
                "job_cluster_key",
                cluster_list,
            )
            .rows(cluster_rows),
        )
        .list(
            FormList::new(
                "environments",
                "Entornos serverless",
                "environment_key",
                vec![
                    FormField::new("environment_key", "Nombre del entorno", FormKind::Text)
                        .required(),
                    FormField::new("environment_version", "Versión del entorno", FormKind::Text)
                        .placeholder("2"),
                    FormField::new("client", "Cliente (obsoleto)", FormKind::Text)
                        .help("Las versiones anteriores a «Versión del entorno»."),
                    FormField::new("dependencies", "Dependencias", FormKind::Textarea)
                        .help("Una por línea, como en requirements.txt.")
                        .placeholder("pandas==2.2.2"),
                    FormField::new(
                        "env_extra",
                        "Otros atributos del entorno (JSON)",
                        FormKind::code("json"),
                    )
                    .help(KEEP_JSON),
                ],
            )
            .rows(env_rows),
        );

    let notifications =
        FormSection::new("notifications", "Notificaciones").fields(notification_fields(sources));

    let git = FormSection::new("git", "Git").fields(vec![
        FormField::new("git_url", "URL del repositorio", FormKind::Text)
            .placeholder("https://github.com/empresa/etl")
            .help("Vacío: el job no usa un repositorio Git."),
        FormField::new(
            "git_provider",
            "Proveedor",
            FormKind::select(&[
                ("gitHub", "GitHub"),
                ("gitHubEnterprise", "GitHub Enterprise"),
                ("bitbucketCloud", "Bitbucket Cloud"),
                ("bitbucketServer", "Bitbucket Server"),
                ("azureDevOpsServices", "Azure DevOps"),
                ("gitLab", "GitLab"),
                ("gitLabEnterpriseEdition", "GitLab Enterprise Edition"),
                ("awsCodeCommit", "AWS CodeCommit"),
            ]),
        ),
        FormField::new(
            "git_ref_kind",
            "Referencia",
            FormKind::select(&[
                ("branch", "Rama"),
                ("tag", "Etiqueta"),
                ("commit", "Commit"),
            ]),
        )
        .default(json!("branch")),
        FormField::new("git_ref", "Rama, etiqueta o commit", FormKind::Text).placeholder("main"),
    ]);

    let acl_known = values
        .get("permissions_read")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let permissions = FormSection::new("permissions", "Permisos")
        .list(
            FormList::new(
                "permissions",
                "Permisos",
                "principal",
                vec![
                    FormField::new(
                        "principal_type",
                        "Tipo",
                        FormKind::select(&[
                            ("user", "Usuario"),
                            ("group", "Grupo"),
                            ("service_principal", "Service principal"),
                        ]),
                    )
                    .required()
                    .default(json!("user")),
                    FormField::new(
                        "principal",
                        "Usuario, grupo o service principal",
                        FormKind::Text,
                    )
                    .required(),
                    FormField::new("level", "Permiso", permission_level())
                        .required()
                        .default(json!("CAN_VIEW")),
                ],
            )
            .rows(acl_direct)
            .read_only(!acl_known),
        )
        .list(
            FormList::new(
                "inherited_permissions",
                "Permisos heredados",
                "principal",
                vec![
                    FormField::new(
                        "principal_type",
                        "Tipo",
                        FormKind::select(&[
                            ("user", "Usuario"),
                            ("group", "Grupo"),
                            ("service_principal", "Service principal"),
                        ]),
                    )
                    .read_only(true),
                    FormField::new(
                        "principal",
                        "Usuario, grupo o service principal",
                        FormKind::Text,
                    )
                    .read_only(true),
                    FormField::new("level", "Permiso", permission_level()).read_only(true),
                    FormField::new("from", "Heredado de", FormKind::Text).read_only(true),
                ],
            )
            .rows(acl_inherited)
            .read_only(true),
        );

    let advanced = FormSection::new("advanced", "Avanzado").field(
        FormField::new("settings_extra", "Otros ajustes (JSON)", FormKind::code("json")).help(
            "Los ajustes del job que no tienen un campo propio (deployment, políticas…), como los toma la API; se guardan tal cual.",
        ),
    );

    let mut read_only = false;
    if let Some(c) = cur {
        if may_manage(c.acl.as_deref(), me) == Some(false) {
            read_only = true;
            notes.insert(0, MANAGE_ONLY.to_string());
        }
    }
    ObjectForm {
        sections: vec![
            general,
            tasks,
            schedule,
            parameters,
            compute,
            notifications,
            git,
            permissions,
            advanced,
        ],
        values,
        choices: sources.lists.clone(),
        notes,
        read_only,
    }
}

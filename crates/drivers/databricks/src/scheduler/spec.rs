//! The job the form says: its settings (and direct permissions) built from
//! the designer's values, checked. What the form has no field for comes
//! from its "Otros … (JSON)" fields and, inside a part the form shows only
//! in part (a schedule, a task's type, notifications), from the job as the
//! workspace has it, so a save never drops what DBine doesn't know.

use super::lines::{
    parse_args, parse_emails, parse_libraries, parse_list, parse_map, parse_rules,
    parse_subscribers, rule,
};
use super::{bad, checked_id, checked_key, rows, shown, Vals};
use crate::Json;
use dbine_driver::{FormValues, Result};
use serde_json::{json, Map};
use std::collections::{BTreeSet, HashMap};

/// The job's settings the form has fields for.
pub(crate) const JOB_KEYS: &[&str] = &[
    "name",
    "description",
    "tags",
    "run_as",
    "edit_mode",
    "performance_target",
    "budget_policy_id",
    "schedule",
    "trigger",
    "continuous",
    "max_concurrent_runs",
    "queue",
    "timeout_seconds",
    "health",
    "parameters",
    "tasks",
    "job_clusters",
    "environments",
    "email_notifications",
    "webhook_notifications",
    "notification_settings",
    "git_source",
];
/// Settings the API answers but ignores in a write (always `MULTI_TASK`).
pub(crate) const IGNORED: &[&str] = &["format"];
/// A task's attributes the form has fields for, whatever its type.
pub(crate) const TASK_COMMON: &[&str] = &[
    "task_key",
    "description",
    "disabled",
    "depends_on",
    "run_if",
    "timeout_seconds",
    "max_retries",
    "min_retry_interval_millis",
    "retry_on_timeout",
    "email_notifications",
    "webhook_notifications",
    "notification_settings",
    "health",
    "disable_auto_optimization",
];
/// Where a task that runs on a cluster or serverless runs, and its libraries.
pub(crate) const COMPUTE_KEYS: &[&str] = &[
    "existing_cluster_id",
    "job_cluster_key",
    "new_cluster",
    "environment_key",
    "libraries",
];
/// A cluster spec's attributes the form has fields for.
pub(crate) const CLUSTER_KEYS: &[&str] = &[
    "spark_version",
    "node_type_id",
    "driver_node_type_id",
    "num_workers",
    "autoscale",
    "data_security_mode",
    "single_user_name",
    "policy_id",
    "instance_pool_id",
    "driver_instance_pool_id",
    "runtime_engine",
    "spark_conf",
    "spark_env_vars",
    "custom_tags",
];
/// Mail lists: the API's key and the form's.
pub(crate) const EMAIL_KEYS: [(&str, &str); 5] = [
    ("on_start", "email_on_start"),
    ("on_success", "email_on_success"),
    ("on_failure", "email_on_failure"),
    (
        "on_duration_warning_threshold_exceeded",
        "email_on_duration_warning",
    ),
    (
        "on_streaming_backlog_exceeded",
        "email_on_streaming_backlog",
    ),
];
/// Notification destinations: the API's key and the form's.
pub(crate) const HOOK_KEYS: [(&str, &str); 5] = [
    ("on_start", "hook_on_start"),
    ("on_success", "hook_on_success"),
    ("on_failure", "hook_on_failure"),
    (
        "on_duration_warning_threshold_exceeded",
        "hook_on_duration_warning",
    ),
    ("on_streaming_backlog_exceeded", "hook_on_streaming_backlog"),
];

/// The task types the form knows and the attribute that holds each.
const TYPES: [(&str, &str); 15] = [
    ("notebook", "notebook_task"),
    ("python_script", "spark_python_task"),
    ("python_wheel", "python_wheel_task"),
    ("jar", "spark_jar_task"),
    ("spark_submit", "spark_submit_task"),
    ("sql_query", "sql_task"),
    ("sql_file", "sql_task"),
    ("sql_dashboard", "sql_task"),
    ("sql_alert", "sql_task"),
    ("dbt", "dbt_task"),
    ("pipeline", "pipeline_task"),
    ("run_job", "run_job_task"),
    ("condition", "condition_task"),
    ("for_each", "for_each_task"),
    ("dashboard", "dashboard_task"),
];
const TYPE_FIELDS: [&str; 12] = [
    "notebook_task",
    "spark_python_task",
    "python_wheel_task",
    "spark_jar_task",
    "spark_submit_task",
    "sql_task",
    "dbt_task",
    "pipeline_task",
    "run_job_task",
    "condition_task",
    "for_each_task",
    "dashboard_task",
];
const TRIGGER_KNOWN: [&str; 5] = [
    "pause_status",
    "periodic",
    "file_arrival",
    "table_update",
    "table",
];
const CONDITION_OPS: [&str; 6] = [
    "EQUAL_TO",
    "NOT_EQUAL",
    "GREATER_THAN",
    "GREATER_THAN_OR_EQUAL",
    "LESS_THAN",
    "LESS_THAN_OR_EQUAL",
];
const RUN_IF: [&str; 6] = [
    "ALL_SUCCESS",
    "AT_LEAST_ONE_SUCCESS",
    "NONE_FAILED",
    "ALL_DONE",
    "AT_LEAST_ONE_FAILED",
    "ALL_FAILED",
];
const LEVELS: [&str; 4] = ["IS_OWNER", "CAN_MANAGE", "CAN_MANAGE_RUN", "CAN_VIEW"];

/// The task's type as the form names it.
pub(crate) fn task_type(t: &Json) -> &'static str {
    if let Some(sql) = t.get("sql_task") {
        return if sql.get("query").is_some() {
            "sql_query"
        } else if sql.get("file").is_some() {
            "sql_file"
        } else if sql.get("dashboard").is_some() {
            "sql_dashboard"
        } else if sql.get("alert").is_some() {
            "sql_alert"
        } else {
            "other"
        };
    }
    TYPES
        .iter()
        .find(|(_, f)| t.get(*f).is_some())
        .map_or("other", |(ty, _)| *ty)
}

fn type_field(ty: &str) -> Option<&'static str> {
    TYPES.iter().find(|(t, _)| *t == ty).map(|(_, f)| *f)
}

/// Types that run on a cluster or serverless (and take libraries).
pub(crate) fn uses_compute(ty: &str) -> bool {
    matches!(
        ty,
        "notebook" | "python_script" | "python_wheel" | "jar" | "spark_submit" | "dbt"
    )
}

/// The attributes of a task of type `ty` the form has fields for.
pub(crate) fn task_modeled(ty: &str) -> Vec<&'static str> {
    let mut m = TASK_COMMON.to_vec();
    m.extend(type_field(ty));
    if uses_compute(ty) {
        m.extend_from_slice(COMPUTE_KEYS);
    }
    m
}

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

/// Nothing to write: null, "", {} or [].
pub(crate) fn blank(v: &Json) -> bool {
    match v {
        Json::Null => true,
        Json::String(s) => s.is_empty(),
        Json::Object(o) => o.is_empty(),
        Json::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// Sets `k` to `v`, or removes it when there's nothing to write.
fn put(m: &mut Map<String, Json>, k: &str, v: Option<Json>) {
    match v.filter(|v| !blank(v)) {
        Some(v) => {
            m.insert(k.into(), v);
        }
        None => {
            m.remove(k);
        }
    }
}

/// `base` (an object, or nothing) without the `known` keys: what the form
/// doesn't show of a part it shows.
fn overlay(base: Option<&Json>, known: &[&str]) -> Map<String, Json> {
    let mut m = base.and_then(Json::as_object).cloned().unwrap_or_default();
    for k in known {
        m.remove(*k);
    }
    m
}

/// A "(JSON)" field: an object, without the keys that have fields.
fn extras(v: Vals, key: &str, label: &str, forbidden: &[&str]) -> Result<Map<String, Json>> {
    let t = v.raw(key);
    if t.trim().is_empty() {
        return Ok(Map::new());
    }
    let parsed: Json = serde_json::from_str(t.trim())
        .map_err(|e| bad(format!("«{label}» no es un objeto JSON válido: {e}")))?;
    let Json::Object(m) = parsed else {
        return Err(bad(format!("«{label}» tiene que ser un objeto JSON.")));
    };
    if let Some(k) = m.keys().find(|k| forbidden.contains(&k.as_str())) {
        return Err(bad(format!(
            "«{label}» no puede tener «{}»: se elige en su propio campo.",
            shown(k)
        )));
    }
    Ok(m)
}

fn one_of(v: &str, allowed: &[&str], label: &str) -> Result<String> {
    if allowed.contains(&v) {
        Ok(v.to_string())
    } else {
        Err(bad(format!(
            "«{label}»: «{}» no es un valor válido.",
            shown(v)
        )))
    }
}

/// An enum value of the API (letters, digits, `_`), as it is.
fn api_word(v: &str, label: &str) -> Result<String> {
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        Ok(v.to_string())
    } else {
        Err(bad(format!(
            "«{label}»: «{}» no es un valor válido.",
            shown(v)
        )))
    }
}

fn text_opt(v: Vals, k: &str) -> Option<Json> {
    v.opt(k).map(Json::String)
}

/// Text kept as typed (descriptions), `None` when blank.
fn raw_opt(v: Vals, k: &str) -> Option<Json> {
    let r = v.raw(k);
    (!r.trim().is_empty()).then_some(Json::String(r))
}

fn id_opt(v: Vals, k: &str, label: &str) -> Result<Option<Json>> {
    v.opt(k)
        .map(|id| checked_id(&id, label).map(Json::String))
        .transpose()
}

fn required(v: Vals, k: &str, what: &str, label: &str) -> Result<String> {
    v.opt(k)
        .ok_or_else(|| bad(format!("{what}: falta «{label}».")))
}

fn flag(on: bool) -> Option<Json> {
    on.then_some(Json::Bool(true))
}

/// Mail lists and destinations for each event (a job's or a task's).
fn notifications(
    m: &mut Map<String, Json>,
    v: Vals,
    base: Option<&Json>,
    settings_keys: &[(&str, &str)],
) -> Result<()> {
    let b = |k: &str| base.and_then(|b| b.get(k));
    let mut email = overlay(b("email_notifications"), &EMAIL_KEYS.map(|(api, _)| api));
    for (api, key) in EMAIL_KEYS {
        let label = match key {
            "email_on_start" => "Correo al empezar",
            "email_on_success" => "Correo al terminar bien",
            "email_on_failure" => "Correo al fallar",
            "email_on_duration_warning" => "Correo si supera la duración esperada",
            _ => "Correo si se acumula backlog de streaming",
        };
        let list = parse_emails(&v.raw(key), label)?;
        put(&mut email, api, Some(json!(list)));
    }
    put(m, "email_notifications", Some(Json::Object(email)));
    let mut hooks = overlay(b("webhook_notifications"), &HOOK_KEYS.map(|(api, _)| api));
    for (api, key) in HOOK_KEYS {
        let ids = v
            .strings(key)
            .iter()
            .map(|id| checked_id(id, "Destino de notificación").map(|id| json!({ "id": id })))
            .collect::<Result<Vec<_>>>()?;
        put(&mut hooks, api, Some(json!(ids)));
    }
    put(m, "webhook_notifications", Some(Json::Object(hooks)));
    let known: Vec<&str> = settings_keys.iter().map(|(api, _)| *api).collect();
    let mut ns = overlay(b("notification_settings"), &known);
    for (api, key) in settings_keys {
        put(&mut ns, api, flag(v.bool(key)));
    }
    put(m, "notification_settings", Some(Json::Object(ns)));
    Ok(())
}

// ---------------------------------------------------------------------------
// Keys of the job's lists, renames included
// ---------------------------------------------------------------------------

/// The keys a list's rows end up with, and the keys they had before an
/// edit renamed them: a reference may still name a row as it was.
#[derive(Debug, Default)]
pub(crate) struct Keys {
    finals: Vec<String>,
    renamed: HashMap<String, String>,
}

impl Keys {
    /// The rows' keys, checked: valid and unique. `orig` is the key a row
    /// had on the server (the row's `orig_key`, when it names one of
    /// `existing`).
    fn of(
        list: &[Vals],
        key: &str,
        what: &str,
        existing: &[String],
    ) -> Result<(Keys, Vec<Option<String>>)> {
        let mut k = Keys::default();
        let mut origs = Vec::new();
        let mut claimed = BTreeSet::new();
        for r in list {
            let typed = r
                .opt(key)
                .ok_or_else(|| bad(format!("{what}: cada uno necesita un nombre.")))?;
            let name = checked_key(&typed, what)?;
            if k.finals.contains(&name) {
                return Err(bad(format!(
                    "{what}: «{}» está dos veces; cada uno necesita un nombre distinto.",
                    shown(&name)
                )));
            }
            let orig = r
                .opt("orig_key")
                .filter(|o| existing.contains(o) && claimed.insert(o.clone()));
            if let Some(o) = &orig {
                if *o != name {
                    k.renamed.insert(o.clone(), name.clone());
                }
            }
            origs.push(orig);
            k.finals.push(name);
        }
        Ok((k, origs))
    }

    pub(crate) fn resolve(&self, name: &str) -> Option<String> {
        let n = name.trim();
        if self.finals.iter().any(|f| f == n) {
            return Some(n.to_string());
        }
        self.renamed.get(n).cloned()
    }
}

fn existing_keys(s: Option<&Json>, list: &str, key: &str) -> Vec<String> {
    s.and_then(|s| s.get(list))
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| x.get(key).and_then(Json::as_str).map(str::to_string))
        .collect()
}

fn find<'a>(s: Option<&'a Json>, list: &str, key: &str, value: &str) -> Option<&'a Json> {
    s?.get(list)?
        .as_array()?
        .iter()
        .find(|x| x.get(key).and_then(Json::as_str) == Some(value))
}

// ---------------------------------------------------------------------------
// Clusters, tasks
// ---------------------------------------------------------------------------

/// A cluster spec from its fields. `autoscale`: a range of workers instead
/// of a number.
fn cluster(r: Vals, base: Option<&Json>, autoscale: bool, what: &str) -> Result<Json> {
    let label = format!("{what}: Otros atributos del clúster (JSON)");
    let mut c = extras(r, "cluster_extra", &label, CLUSTER_KEYS)?;
    for (k, l) in [
        ("spark_version", "Versión de Databricks Runtime"),
        ("node_type_id", "Tipo de nodo de los workers"),
        ("driver_node_type_id", "Tipo de nodo del driver"),
        ("policy_id", "Política de clúster"),
        ("instance_pool_id", "Pool de instancias"),
        ("driver_instance_pool_id", "Pool de instancias del driver"),
    ] {
        put(&mut c, k, id_opt(r, k, l)?);
    }
    put(&mut c, "single_user_name", text_opt(r, "single_user_name"));
    for (k, l) in [
        ("data_security_mode", "Modo de acceso"),
        ("runtime_engine", "Motor"),
    ] {
        let v = r.opt(k).map(|x| api_word(&x, l)).transpose()?;
        put(&mut c, k, v.map(Json::String));
    }
    for (k, l) in [
        ("spark_conf", "Configuración de Spark"),
        ("spark_env_vars", "Variables de entorno"),
        ("custom_tags", "Etiquetas del clúster"),
    ] {
        put(&mut c, k, parse_map(&r.raw(k), l)?.map(Json::Object));
    }
    if autoscale {
        let min = r.int("autoscale_min", "Mínimo de workers", 0, 100_000)?;
        let max = r.int("autoscale_max", "Máximo de workers", 1, 100_000)?;
        let (Some(min), Some(max)) = (min, max) else {
            return Err(bad(format!(
                "{what}: el escalado automático necesita el mínimo y el máximo de workers."
            )));
        };
        if min > max {
            return Err(bad(format!(
                "{what}: el mínimo de workers ({min}) es mayor que el máximo ({max})."
            )));
        }
        let mut a = overlay(
            base.and_then(|b| b.get("autoscale")),
            &["min_workers", "max_workers"],
        );
        a.insert("min_workers".into(), json!(min));
        a.insert("max_workers".into(), json!(max));
        c.insert("autoscale".into(), Json::Object(a));
    } else {
        let n = r.int("num_workers", "Workers", 0, 100_000)?;
        put(&mut c, "num_workers", n.map(|n| json!(n)));
    }
    Ok(Json::Object(c))
}

/// What a task's references point at.
struct Refs<'a> {
    tasks: &'a Keys,
    clusters: &'a Keys,
    envs: &'a Keys,
}

fn sql_part(
    sql: &mut Map<String, Json>,
    base: Option<&Json>,
    part: &str,
    known: &[&str],
    set: Vec<(&str, Option<Json>)>,
) {
    let mut p = overlay(base.and_then(|b| b.get(part)), known);
    for (k, v) in set {
        put(&mut p, k, v);
    }
    sql.insert(part.into(), Json::Object(p));
}

/// A task from its row. `base`: the task as the server has it.
fn task(r: Vals, key: &str, base: Option<&Json>, refs: &Refs) -> Result<Json> {
    let what = format!("La tarea «{}»", shown(key));
    let ty = r.opt("type").unwrap_or_else(|| "notebook".into());
    if ty != "other" && type_field(&ty).is_none() {
        return Err(bad(format!(
            "{what}: «{}» no es un tipo de tarea.",
            shown(&ty)
        )));
    }
    let same = base.is_some_and(|b| task_type(b) == ty);
    let base_body = |f: &str| {
        if same {
            base.and_then(|b| b.get(f))
        } else {
            None
        }
    };
    let mut forbidden: Vec<&str> = TASK_COMMON.to_vec();
    forbidden.extend(TYPE_FIELDS);
    if uses_compute(&ty) {
        forbidden.extend_from_slice(COMPUTE_KEYS);
    }
    let mut t = extras(
        r,
        "task_extra",
        &format!("{what}: Otros atributos (JSON)"),
        &forbidden,
    )?;
    if ty == "other" && !t.keys().any(|k| k.ends_with("_task")) {
        return Err(bad(format!(
            "{what} es de otro tipo: su definición va en «Otros atributos (JSON)» (por ejemplo, {{\"power_bi_task\": {{…}}}})."
        )));
    }
    t.insert("task_key".into(), json!(key));
    put(&mut t, "description", raw_opt(r, "description"));
    put(&mut t, "disabled", flag(r.bool("disabled")));
    let mut deps = Vec::new();
    for (field, outcome) in [
        ("depends_on", None),
        ("depends_true", Some("true")),
        ("depends_false", Some("false")),
    ] {
        for d in r.strings(field) {
            let to = refs.tasks.resolve(&d).ok_or_else(|| {
                bad(format!(
                    "{what} depende de «{}», que no está en el job.",
                    shown(&d)
                ))
            })?;
            if to == key {
                return Err(bad(format!("{what} no puede depender de sí misma.")));
            }
            deps.push(match outcome {
                Some(o) => json!({ "task_key": to, "outcome": o }),
                None => json!({ "task_key": to }),
            });
        }
    }
    put(&mut t, "depends_on", Some(json!(deps)));
    let run_if = r
        .opt("run_if")
        .map(|x| one_of(&x, &RUN_IF, "Ejecutar si"))
        .transpose()?;
    put(&mut t, "run_if", run_if.map(Json::String));

    // The type's own attributes.
    let source = || -> Result<Option<Json>> {
        r.opt("source")
            .map(|s| one_of(&s, &["WORKSPACE", "GIT"], "Origen").map(Json::String))
            .transpose()
    };
    let need = |k: &str, label: &str| required(r, k, &what, label);
    let body = |f: &str, known: &[&str], set: Vec<(&str, Option<Json>)>| {
        let mut b = overlay(base_body(f), known);
        for (k, v) in set {
            put(&mut b, k, v);
        }
        Json::Object(b)
    };
    let typed = match ty.as_str() {
        "notebook" => Some(body(
            "notebook_task",
            &["notebook_path", "source", "base_parameters", "warehouse_id"],
            vec![
                (
                    "notebook_path",
                    Some(json!(need("notebook_path", "Notebook")?)),
                ),
                ("source", source()?),
                (
                    "base_parameters",
                    parse_map(&r.raw("base_parameters"), "Parámetros")?.map(Json::Object),
                ),
                ("warehouse_id", id_opt(r, "warehouse_id", "SQL warehouse")?),
            ],
        )),
        "python_script" => Some(body(
            "spark_python_task",
            &["python_file", "source", "parameters"],
            vec![
                (
                    "python_file",
                    Some(json!(need("python_file", "Archivo de Python")?)),
                ),
                ("source", source()?),
                (
                    "parameters",
                    parse_args(&r.raw("parameters"), "Argumentos")?.map(Json::Array),
                ),
            ],
        )),
        "python_wheel" => {
            let args = parse_args(&r.raw("parameters"), "Argumentos")?;
            let named = parse_map(&r.raw("named_parameters"), "Argumentos con nombre")?;
            if args.as_ref().is_some_and(|a| !a.is_empty())
                && named.as_ref().is_some_and(|n| !n.is_empty())
            {
                return Err(bad(format!(
                    "{what}: un wheel toma argumentos o argumentos con nombre, no los dos."
                )));
            }
            Some(body(
                "python_wheel_task",
                &[
                    "package_name",
                    "entry_point",
                    "parameters",
                    "named_parameters",
                ],
                vec![
                    (
                        "package_name",
                        Some(json!(need("package_name", "Paquete")?)),
                    ),
                    (
                        "entry_point",
                        Some(json!(need("entry_point", "Punto de entrada")?)),
                    ),
                    ("parameters", args.map(Json::Array)),
                    ("named_parameters", named.map(Json::Object)),
                ],
            ))
        }
        "jar" => Some(body(
            "spark_jar_task",
            &["main_class_name", "parameters"],
            vec![
                (
                    "main_class_name",
                    Some(json!(need("main_class_name", "Clase principal")?)),
                ),
                (
                    "parameters",
                    parse_args(&r.raw("parameters"), "Argumentos")?.map(Json::Array),
                ),
            ],
        )),
        "spark_submit" => Some(body(
            "spark_submit_task",
            &["parameters"],
            vec![(
                "parameters",
                parse_args(&r.raw("parameters"), "Argumentos")?.map(Json::Array),
            )],
        )),
        "sql_query" | "sql_file" | "sql_dashboard" | "sql_alert" => {
            let warehouse = checked_id(&need("warehouse_id", "SQL warehouse")?, "SQL warehouse")?;
            let base_sql = base_body("sql_task");
            let mut sql = overlay(
                base_sql,
                &[
                    "warehouse_id",
                    "parameters",
                    "query",
                    "file",
                    "dashboard",
                    "alert",
                ],
            );
            sql.insert("warehouse_id".into(), json!(warehouse));
            put(
                &mut sql,
                "parameters",
                parse_map(&r.raw("sql_parameters"), "Parámetros")?.map(Json::Object),
            );
            let subs = |r: Vals| parse_subscribers(&r.raw("subscriptions"), "Suscriptores");
            match ty.as_str() {
                "sql_query" => sql_part(
                    &mut sql,
                    base_sql,
                    "query",
                    &["query_id"],
                    vec![(
                        "query_id",
                        Some(json!(checked_id(
                            &need("query_id", "Id de la consulta")?,
                            "Id de la consulta"
                        )?)),
                    )],
                ),
                "sql_file" => sql_part(
                    &mut sql,
                    base_sql,
                    "file",
                    &["path", "source"],
                    vec![
                        ("path", Some(json!(need("sql_path", "Archivo SQL")?))),
                        ("source", source()?),
                    ],
                ),
                "sql_dashboard" => sql_part(
                    &mut sql,
                    base_sql,
                    "dashboard",
                    &[
                        "dashboard_id",
                        "custom_subject",
                        "pause_subscriptions",
                        "subscriptions",
                    ],
                    vec![
                        (
                            "dashboard_id",
                            Some(json!(checked_id(
                                &need("sql_dashboard_id", "Id del dashboard")?,
                                "Id del dashboard"
                            )?)),
                        ),
                        ("custom_subject", raw_opt(r, "custom_subject")),
                        ("pause_subscriptions", flag(r.bool("pause_subscriptions"))),
                        ("subscriptions", Some(json!(subs(r)?))),
                    ],
                ),
                _ => sql_part(
                    &mut sql,
                    base_sql,
                    "alert",
                    &["alert_id", "pause_subscriptions", "subscriptions"],
                    vec![
                        (
                            "alert_id",
                            Some(json!(checked_id(
                                &need("alert_id", "Id de la alerta")?,
                                "Id de la alerta"
                            )?)),
                        ),
                        ("pause_subscriptions", flag(r.bool("pause_subscriptions"))),
                        ("subscriptions", Some(json!(subs(r)?))),
                    ],
                ),
            }
            Some(Json::Object(sql))
        }
        "dbt" => {
            let commands = parse_list(&r.raw("dbt_commands"));
            if commands.is_empty() {
                return Err(bad(format!("{what}: falta «Comandos de dbt».")));
            }
            if let Some(c) = commands
                .iter()
                .find(|c| c.split_whitespace().next() != Some("dbt"))
            {
                return Err(bad(format!(
                    "{what}: el comando «{}» no empieza con «dbt».",
                    shown(c)
                )));
            }
            Some(body(
                "dbt_task",
                &[
                    "commands",
                    "project_directory",
                    "profiles_directory",
                    "catalog",
                    "schema",
                    "warehouse_id",
                    "source",
                ],
                vec![
                    ("commands", Some(json!(commands))),
                    ("project_directory", text_opt(r, "project_directory")),
                    ("profiles_directory", text_opt(r, "profiles_directory")),
                    ("catalog", text_opt(r, "dbt_catalog")),
                    ("schema", text_opt(r, "dbt_schema")),
                    ("warehouse_id", id_opt(r, "warehouse_id", "SQL warehouse")?),
                    ("source", source()?),
                ],
            ))
        }
        "pipeline" => Some(body(
            "pipeline_task",
            &["pipeline_id", "full_refresh"],
            vec![
                (
                    "pipeline_id",
                    Some(json!(checked_id(
                        &need("pipeline_id", "Pipeline")?,
                        "Pipeline"
                    )?)),
                ),
                ("full_refresh", flag(r.bool("full_refresh"))),
            ],
        )),
        "run_job" => {
            let id = need("run_job_id", "Job")?;
            let n = crate::jobs::job_id(&id).ok_or_else(|| {
                bad(format!(
                    "{what}: «{}» no es un id de job de Databricks.",
                    shown(&id)
                ))
            })?;
            Some(body(
                "run_job_task",
                &["job_id", "job_parameters"],
                vec![
                    ("job_id", Some(json!(n))),
                    (
                        "job_parameters",
                        parse_map(&r.raw("job_parameters"), "Parámetros del job")?
                            .map(Json::Object),
                    ),
                ],
            ))
        }
        "condition" => {
            let op = one_of(
                &need("condition_op", "Operador")?,
                &CONDITION_OPS,
                "Operador",
            )?;
            let side = |k: &str, label: &str| -> Result<Json> {
                let v = r.raw(k);
                if v.trim().is_empty() {
                    return Err(bad(format!("{what}: falta «{label}».")));
                }
                Ok(json!(v.trim()))
            };
            Some(body(
                "condition_task",
                &["op", "left", "right"],
                vec![
                    ("op", Some(json!(op))),
                    ("left", Some(side("condition_left", "Valor izquierdo")?)),
                    ("right", Some(side("condition_right", "Valor derecho")?)),
                ],
            ))
        }
        "for_each" => {
            let inputs = r.raw("foreach_inputs");
            if inputs.trim().is_empty() {
                return Err(bad(format!("{what}: falta «Elementos».")));
            }
            let inner = r.raw("foreach_task");
            let inner: Json = serde_json::from_str(inner.trim()).map_err(|e| {
                bad(format!(
                    "{what}: «Tarea de cada iteración (JSON)» no es un objeto JSON válido: {e}"
                ))
            })?;
            if !inner.get("task_key").is_some_and(Json::is_string) {
                return Err(bad(format!(
                    "{what}: la tarea de cada iteración necesita su «task_key»."
                )));
            }
            Some(body(
                "for_each_task",
                &["inputs", "concurrency", "task"],
                vec![
                    ("inputs", Some(json!(inputs.trim()))),
                    (
                        "concurrency",
                        r.int("foreach_concurrency", "Iteraciones a la vez", 1, 100)?
                            .map(|n| json!(n)),
                    ),
                    ("task", Some(inner)),
                ],
            ))
        }
        "dashboard" => {
            let mut sub = overlay(
                base_body("dashboard_task").and_then(|b| b.get("subscription")),
                &["custom_subject", "paused", "subscribers"],
            );
            put(&mut sub, "custom_subject", raw_opt(r, "custom_subject"));
            put(&mut sub, "paused", flag(r.bool("pause_subscriptions")));
            put(
                &mut sub,
                "subscribers",
                Some(json!(parse_subscribers(
                    &r.raw("subscriptions"),
                    "Suscriptores"
                )?)),
            );
            Some(body(
                "dashboard_task",
                &["dashboard_id", "warehouse_id", "subscription"],
                vec![
                    (
                        "dashboard_id",
                        Some(json!(checked_id(
                            &need("dashboard_id", "Id del dashboard")?,
                            "Id del dashboard"
                        )?)),
                    ),
                    ("warehouse_id", id_opt(r, "warehouse_id", "SQL warehouse")?),
                    ("subscription", Some(Json::Object(sub))),
                ],
            ))
        }
        _ => None,
    };
    if let (Some(f), Some(b)) = (type_field(&ty), typed) {
        t.insert(f.into(), b);
    }

    // Where it runs.
    if uses_compute(&ty) {
        match r.opt("compute").as_deref().unwrap_or("serverless") {
            "serverless" => {
                let env = match r.opt("environment_key") {
                    None => None,
                    Some(e) => Some(json!(refs.envs.resolve(&e).ok_or_else(|| {
                        bad(format!(
                            "{what} usa el entorno «{}», que no está en el job.",
                            shown(&e)
                        ))
                    })?)),
                };
                put(&mut t, "environment_key", env);
            }
            "existing" => {
                let c = checked_id(&need("existing_cluster_id", "Clúster")?, "Clúster")?;
                t.insert("existing_cluster_id".into(), json!(c));
            }
            "job_cluster" => {
                let c = need("job_cluster_key", "Clúster del job")?;
                let c = refs.clusters.resolve(&c).ok_or_else(|| {
                    bad(format!(
                        "{what} usa el clúster del job «{}», que no está en el job.",
                        shown(&c)
                    ))
                })?;
                t.insert("job_cluster_key".into(), json!(c));
            }
            m @ ("new_cluster" | "new_cluster_autoscale") => {
                let spec = cluster(
                    r,
                    base.and_then(|b| b.get("new_cluster")),
                    m == "new_cluster_autoscale",
                    &what,
                )?;
                t.insert("new_cluster".into(), spec);
            }
            other => {
                return Err(bad(format!(
                    "{what}: «{}» no es un cómputo válido.",
                    shown(other)
                )))
            }
        }
        put(
            &mut t,
            "libraries",
            parse_libraries(&r.raw("libraries"))?.map(Json::Array),
        );
    }

    // Retries, limits, notifications, health.
    let num = |k: &str, label: &str, min: i64| -> Result<Option<Json>> {
        Ok(r.int(k, label, min, i64::from(i32::MAX))?.map(|n| json!(n)))
    };
    put(
        &mut t,
        "timeout_seconds",
        num("timeout_seconds", "Tiempo máximo", 0)?,
    );
    put(&mut t, "max_retries", num("max_retries", "Reintentos", -1)?);
    put(
        &mut t,
        "min_retry_interval_millis",
        num(
            "min_retry_interval_millis",
            "Espera entre reintentos (ms)",
            0,
        )?,
    );
    put(&mut t, "retry_on_timeout", flag(r.bool("retry_on_timeout")));
    notifications(
        &mut t,
        r,
        base,
        &[
            ("no_alert_for_skipped_runs", "no_alert_skipped"),
            ("no_alert_for_canceled_runs", "no_alert_canceled"),
            ("alert_on_last_attempt", "alert_on_last_attempt"),
        ],
    )?;
    let rules = parse_rules(&r.raw("health"), &what)?;
    let mut health = overlay(base.and_then(|b| b.get("health")), &["rules"]);
    put(&mut health, "rules", Some(json!(rules)));
    put(&mut t, "health", Some(Json::Object(health)));
    put(
        &mut t,
        "disable_auto_optimization",
        flag(r.bool("disable_auto_optimization")),
    );
    Ok(Json::Object(t))
}

/// Tasks that depend on each other in a circle, if any.
fn cycle(tasks: &[Json]) -> Option<String> {
    let deps: HashMap<&str, Vec<&str>> = tasks
        .iter()
        .filter_map(|t| {
            let k = t.get("task_key")?.as_str()?;
            let d = t
                .get("depends_on")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
                .filter_map(|d| d.get("task_key").and_then(Json::as_str))
                .collect();
            Some((k, d))
        })
        .collect();
    // 0: not seen, 1: on the path, 2: done.
    let mut state: HashMap<&str, u8> = HashMap::new();
    fn visit<'a>(
        k: &'a str,
        deps: &HashMap<&'a str, Vec<&'a str>>,
        state: &mut HashMap<&'a str, u8>,
    ) -> Option<String> {
        match state.get(k) {
            Some(1) => return Some(k.to_string()),
            Some(2) => return None,
            _ => {}
        }
        state.insert(k, 1);
        for d in deps.get(k).into_iter().flatten() {
            if let Some(c) = visit(d, deps, state) {
                return Some(c);
            }
        }
        state.insert(k, 2);
        None
    }
    let mut keys: Vec<&str> = deps.keys().copied().collect();
    keys.sort();
    keys.into_iter().find_map(|k| visit(k, &deps, &mut state))
}

// ---------------------------------------------------------------------------
// The job
// ---------------------------------------------------------------------------

/// A quartz cron expression: 6 or 7 fields of its characters.
fn quartz(cron: &str) -> Result<String> {
    let c = cron.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = c.split(' ').count();
    let ok = (6..=7).contains(&n)
        && c.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || " *?,-/#".contains(ch));
    if ok {
        Ok(c)
    } else {
        Err(bad(format!(
            "«Expresión cron (Quartz)»: «{}» no es una expresión de Quartz (seis o siete campos: segundos, minutos, horas, día del mes, mes, día de la semana y año).",
            shown(cron)
        )))
    }
}

/// A Java time zone id (`UTC`, `America/Argentina/Buenos_Aires`, `GMT-03:00`).
fn timezone(tz: &str) -> Result<String> {
    let ok = !tz.is_empty()
        && tz.len() <= 64
        && tz
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_+-:".contains(&b));
    if ok {
        Ok(tz.to_string())
    } else {
        Err(bad(format!(
            "«Zona horaria»: «{}» no es una zona horaria.",
            shown(tz)
        )))
    }
}

/// A wait of a trigger: 0 or at least 60 seconds.
fn trigger_wait(v: Vals, k: &str, label: &str) -> Result<Option<Json>> {
    match v.int(k, label, 0, i64::from(i32::MAX))? {
        Some(n) if n > 0 && n < 60 => Err(bad(format!(
            "«{label}» tiene que ser 0 o al menos 60 segundos (es {n})."
        ))),
        n => Ok(n.map(|n| json!(n))),
    }
}

/// The job's settings as the form says them. `cur`: the settings the
/// workspace has (`None` for a new job).
pub(crate) fn settings(values: &FormValues, cur: Option<&Json>) -> Result<Json> {
    let v = Vals(&values.values);
    let b = |k: &str| cur.and_then(|c| c.get(k));
    let mut s = extras(v, "settings_extra", "Otros ajustes (JSON)", JOB_KEYS)?;
    for k in IGNORED {
        s.remove(*k);
    }

    // General.
    let name = v
        .opt("name")
        .ok_or_else(|| bad("El job necesita un nombre.".into()))?;
    if name.chars().count() > 4096 {
        return Err(bad(
            "El nombre del job puede tener hasta 4096 caracteres.".into()
        ));
    }
    s.insert("name".into(), json!(name));
    put(&mut s, "description", raw_opt(v, "description"));
    let tags = parse_map(&v.raw("tags"), "Etiquetas")?;
    if tags.as_ref().is_some_and(|t| t.len() > 25) {
        return Err(bad("Un job puede tener hasta 25 etiquetas.".into()));
    }
    put(&mut s, "tags", tags.map(Json::Object));
    let mut run_as = overlay(b("run_as"), &["user_name", "service_principal_name"]);
    match v.opt("run_as_mode").as_deref().unwrap_or("owner") {
        "owner" => {}
        mode @ ("user" | "service_principal") => {
            let who = v.opt("run_as_name").ok_or_else(|| {
                bad("«Ejecutar como»: falta el usuario o el service principal.".into())
            })?;
            if who.chars().any(char::is_whitespace) {
                return Err(bad(format!(
                    "«Usuario o service principal»: «{}» no es un nombre válido.",
                    shown(&who)
                )));
            }
            let k = if mode == "user" {
                "user_name"
            } else {
                "service_principal_name"
            };
            run_as.insert(k.into(), json!(who));
        }
        other => {
            return Err(bad(format!(
                "«Ejecutar como»: «{}» no es un valor válido.",
                shown(other)
            )))
        }
    }
    put(&mut s, "run_as", Some(Json::Object(run_as)));
    let edit = v
        .opt("edit_mode")
        .map(|m| one_of(&m, &["EDITABLE", "UI_LOCKED"], "Modo de edición"))
        .transpose()?;
    put(&mut s, "edit_mode", edit.map(Json::String));
    let perf = v
        .opt("performance_target")
        .map(|m| api_word(&m, "Rendimiento (serverless)"))
        .transpose()?;
    put(&mut s, "performance_target", perf.map(Json::String));
    put(
        &mut s,
        "budget_policy_id",
        id_opt(v, "budget_policy_id", "Política de presupuesto")?,
    );

    // How it starts.
    let mode = v.opt("trigger_mode").unwrap_or_else(|| "none".into());
    let cur_mode = cur.map_or("none", super::form::trigger_mode);
    let same = |part: &str| (mode == cur_mode).then(|| b(part)).flatten();
    let pause = json!(if v.bool("paused") {
        "PAUSED"
    } else {
        "UNPAUSED"
    });
    for k in ["schedule", "trigger", "continuous"] {
        s.remove(k);
    }
    match mode.as_str() {
        "none" => {}
        "schedule" => {
            let cron = quartz(
                &v.opt("cron")
                    .ok_or_else(|| bad("La programación necesita su expresión cron.".into()))?,
            )?;
            let tz = timezone(&v.opt("timezone").unwrap_or_else(|| "UTC".into()))?;
            let mut sc = overlay(
                same("schedule"),
                &["quartz_cron_expression", "timezone_id", "pause_status"],
            );
            sc.insert("quartz_cron_expression".into(), json!(cron));
            sc.insert("timezone_id".into(), json!(tz));
            sc.insert("pause_status".into(), pause);
            s.insert("schedule".into(), Json::Object(sc));
        }
        "periodic" | "file_arrival" | "table_update" => {
            let base = same("trigger");
            let mut t = overlay(base, &TRIGGER_KNOWN);
            t.insert("pause_status".into(), pause);
            let part = |k: &str| base.and_then(|b| b.get(k));
            match mode.as_str() {
                "periodic" => {
                    let n = v
                        .int("periodic_interval", "Cada", 1, 100_000)?
                        .ok_or_else(|| bad("«Cada»: falta cada cuánto.".into()))?;
                    let unit = one_of(
                        &v.opt("periodic_unit").unwrap_or_else(|| "DAYS".into()),
                        &["HOURS", "DAYS", "WEEKS"],
                        "Unidad",
                    )?;
                    let mut p = overlay(part("periodic"), &["interval", "unit"]);
                    p.insert("interval".into(), json!(n));
                    p.insert("unit".into(), json!(unit));
                    t.insert("periodic".into(), Json::Object(p));
                }
                "file_arrival" => {
                    let url = v.opt("file_url").ok_or_else(|| {
                        bad("«Ubicación»: falta dónde llegan los archivos.".into())
                    })?;
                    let mut f = overlay(
                        part("file_arrival"),
                        &[
                            "url",
                            "min_time_between_triggers_seconds",
                            "wait_after_last_change_seconds",
                        ],
                    );
                    f.insert("url".into(), json!(url));
                    put(
                        &mut f,
                        "min_time_between_triggers_seconds",
                        trigger_wait(v, "min_between", "Tiempo mínimo entre disparos")?,
                    );
                    put(
                        &mut f,
                        "wait_after_last_change_seconds",
                        trigger_wait(v, "wait_after", "Esperar tras el último cambio")?,
                    );
                    t.insert("file_arrival".into(), Json::Object(f));
                }
                _ => {
                    // The older name of the same trigger stays where it is.
                    let key = if part("table").is_some() {
                        "table"
                    } else {
                        "table_update"
                    };
                    let tables = parse_list(&v.raw("table_names"));
                    if tables.is_empty() {
                        return Err(bad("«Tablas»: falta al menos una tabla.".into()));
                    }
                    let cond = one_of(
                        &v.opt("table_condition")
                            .unwrap_or_else(|| "ANY_UPDATED".into()),
                        &["ANY_UPDATED", "ALL_UPDATED"],
                        "Cuándo",
                    )?;
                    let mut tu = overlay(
                        part(key),
                        &[
                            "table_names",
                            "condition",
                            "min_time_between_triggers_seconds",
                            "wait_after_last_change_seconds",
                        ],
                    );
                    tu.insert("table_names".into(), json!(tables));
                    tu.insert("condition".into(), json!(cond));
                    put(
                        &mut tu,
                        "min_time_between_triggers_seconds",
                        trigger_wait(v, "min_between", "Tiempo mínimo entre disparos")?,
                    );
                    put(
                        &mut tu,
                        "wait_after_last_change_seconds",
                        trigger_wait(v, "wait_after", "Esperar tras el último cambio")?,
                    );
                    t.insert(key.into(), Json::Object(tu));
                }
            }
            s.insert("trigger".into(), Json::Object(t));
        }
        "continuous" => {
            let mut c = overlay(same("continuous"), &["pause_status", "task_retry_mode"]);
            c.insert("pause_status".into(), pause);
            let retry = v
                .opt("continuous_retry")
                .map(|m| one_of(&m, &["ON_FAILURE", "NEVER"], "Reintentos de las tareas"))
                .transpose()?;
            put(&mut c, "task_retry_mode", retry.map(Json::String));
            s.insert("continuous".into(), Json::Object(c));
        }
        "other" => {
            let Some(t) = same("trigger").and_then(Json::as_object) else {
                return Err(bad(
                    "«Otro disparador» solo conserva el disparador que el job ya tiene; elegí cómo empieza."
                        .into(),
                ));
            };
            let mut t = t.clone();
            t.insert("pause_status".into(), pause);
            s.insert("trigger".into(), Json::Object(t));
        }
        other => {
            return Err(bad(format!(
                "«Cómo empieza»: «{}» no es un valor válido.",
                shown(other)
            )))
        }
    }
    put(
        &mut s,
        "max_concurrent_runs",
        v.int(
            "max_concurrent_runs",
            "Máximo de ejecuciones a la vez",
            0,
            1000,
        )?
        .map(|n| json!(n)),
    );
    let mut queue = overlay(b("queue"), &["enabled"]);
    queue.insert("enabled".into(), json!(v.bool("queue")));
    s.insert("queue".into(), Json::Object(queue));
    put(
        &mut s,
        "timeout_seconds",
        v.int(
            "timeout_seconds",
            "Tiempo máximo de cada ejecución",
            0,
            i64::from(i32::MAX),
        )?
        .map(|n| json!(n)),
    );
    let mut rules = Vec::new();
    for r in rows(values, "health") {
        let metric = r.opt("metric").unwrap_or_default();
        let value = r
            .int("value", "Mayor que", 0, i64::MAX)?
            .ok_or_else(|| bad("Reglas de salud: cada regla necesita su umbral.".into()))?;
        let mut rule = rule(&metric, value, "Reglas de salud")?;
        // A comparison the form doesn't show stays as it was.
        if let Some(op) = r.opt("op") {
            rule["op"] = json!(api_word(&op, "Reglas de salud")?);
        }
        rules.push(rule);
    }
    let mut health = overlay(b("health"), &["rules"]);
    put(&mut health, "rules", Some(json!(rules)));
    put(&mut s, "health", Some(Json::Object(health)));

    // Job parameters.
    let mut params = Vec::new();
    let mut seen = BTreeSet::new();
    for r in rows(values, "parameters") {
        let name = r
            .opt("name")
            .ok_or_else(|| bad("Parámetros del job: cada uno necesita un nombre.".into()))?;
        if !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        {
            return Err(bad(format!(
                "Parámetros del job: «{}» no es un nombre válido (letras, números, «_», «-» y «.»).",
                shown(&name)
            )));
        }
        if !seen.insert(name.clone()) {
            return Err(bad(format!(
                "Parámetros del job: «{}» está dos veces.",
                shown(&name)
            )));
        }
        params.push(json!({ "name": name, "default": r.raw("default") }));
    }
    put(&mut s, "parameters", Some(json!(params)));

    // Job clusters and environments, then the tasks that use them.
    let cluster_rows = rows(values, "job_clusters");
    let (clusters, cluster_origs) = Keys::of(
        &cluster_rows,
        "job_cluster_key",
        "Clústeres del job",
        &existing_keys(cur, "job_clusters", "job_cluster_key"),
    )?;
    let mut jcs = Vec::new();
    for ((r, key), orig) in cluster_rows
        .iter()
        .zip(&clusters.finals)
        .zip(&cluster_origs)
    {
        let base = orig
            .as_deref()
            .and_then(|o| find(cur, "job_clusters", "job_cluster_key", o))
            .and_then(|jc| jc.get("new_cluster"));
        let what = format!("El clúster del job «{}»", shown(key));
        let autoscale = r.opt("workers").as_deref() == Some("autoscale");
        jcs.push(json!({
            "job_cluster_key": key,
            "new_cluster": cluster(*r, base, autoscale, &what)?,
        }));
    }
    put(&mut s, "job_clusters", Some(json!(jcs)));

    let env_rows = rows(values, "environments");
    let (envs, _) = Keys::of(
        &env_rows,
        "environment_key",
        "Entornos serverless",
        &existing_keys(cur, "environments", "environment_key"),
    )?;
    let mut list = Vec::new();
    for (r, key) in env_rows.iter().zip(&envs.finals) {
        let what = format!("El entorno «{}»", shown(key));
        let mut spec = extras(
            *r,
            "env_extra",
            &format!("{what}: Otros atributos del entorno (JSON)"),
            &["environment_version", "client", "dependencies"],
        )?;
        put(
            &mut spec,
            "environment_version",
            text_opt(*r, "environment_version"),
        );
        put(&mut spec, "client", text_opt(*r, "client"));
        put(
            &mut spec,
            "dependencies",
            Some(json!(parse_list(&r.raw("dependencies")))),
        );
        list.push(json!({ "environment_key": key, "spec": spec }));
    }
    put(&mut s, "environments", Some(json!(list)));

    let task_rows = rows(values, "tasks");
    let (tasks, task_origs) = Keys::of(
        &task_rows,
        "task_key",
        "Tareas",
        &existing_keys(cur, "tasks", "task_key"),
    )?;
    let refs = Refs {
        tasks: &tasks,
        clusters: &clusters,
        envs: &envs,
    };
    let mut out = Vec::new();
    for ((r, key), orig) in task_rows.iter().zip(&tasks.finals).zip(&task_origs) {
        let base = orig
            .as_deref()
            .and_then(|o| find(cur, "tasks", "task_key", o));
        out.push(task(*r, key, base, &refs)?);
    }
    // A branch of a condition comes from a condition task.
    let conditions: Vec<&str> = out
        .iter()
        .filter(|t| t.get("condition_task").is_some())
        .filter_map(|t| t.get("task_key").and_then(Json::as_str))
        .collect();
    for t in &out {
        for d in t
            .get("depends_on")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            if d.get("outcome").is_some() {
                let to = d.get("task_key").and_then(Json::as_str).unwrap_or_default();
                if !conditions.contains(&to) {
                    return Err(bad(format!(
                        "La tarea «{}» depende de la rama de «{}», que no es una tarea de condición.",
                        shown(t["task_key"].as_str().unwrap_or_default()),
                        shown(to)
                    )));
                }
            }
        }
    }
    if let Some(k) = cycle(&out) {
        return Err(bad(format!(
            "Las dependencias de las tareas forman un ciclo (pasa por «{}»).",
            shown(&k)
        )));
    }
    put(&mut s, "tasks", Some(json!(out)));

    // Notifications.
    notifications(
        &mut s,
        v,
        cur,
        &[
            ("no_alert_for_skipped_runs", "no_alert_skipped"),
            ("no_alert_for_canceled_runs", "no_alert_canceled"),
        ],
    )?;

    // Git.
    s.remove("git_source");
    if let Some(url) = v.opt("git_url") {
        let provider = v
            .opt("git_provider")
            .ok_or_else(|| bad("Git: falta el proveedor del repositorio.".into()))?;
        let provider = api_word(&provider, "Proveedor")?;
        let kind = one_of(
            &v.opt("git_ref_kind").unwrap_or_else(|| "branch".into()),
            &["branch", "tag", "commit"],
            "Referencia",
        )?;
        let r = v
            .opt("git_ref")
            .ok_or_else(|| bad("Git: falta la rama, la etiqueta o el commit.".into()))?;
        let mut g = overlay(
            b("git_source"),
            &[
                "git_url",
                "git_provider",
                "git_branch",
                "git_tag",
                "git_commit",
            ],
        );
        g.insert("git_url".into(), json!(url));
        g.insert("git_provider".into(), json!(provider));
        g.insert(format!("git_{kind}"), json!(r));
        s.insert("git_source".into(), Json::Object(g));
    }
    Ok(Json::Object(s))
}

/// The job's direct permissions as the form says them; `None` when the
/// form couldn't read them (they stay as they are).
pub(crate) fn acl(values: &FormValues) -> Result<Option<Vec<Json>>> {
    if !Vals(&values.values).bool("permissions_read") {
        return Ok(None);
    }
    let mut out: Vec<Json> = Vec::new();
    let mut owners = 0;
    for r in rows(values, "permissions") {
        let who = r.opt("principal").ok_or_else(|| {
            bad("Permisos: cada uno necesita su usuario, grupo o service principal.".into())
        })?;
        if who.chars().any(|c| c == '\n' || c == '\r') {
            return Err(bad(format!(
                "Permisos: «{}» no es un nombre válido.",
                shown(&who)
            )));
        }
        let kind = r.opt("principal_type").unwrap_or_else(|| "user".into());
        let key = match kind.as_str() {
            "user" => "user_name",
            "group" => "group_name",
            "service_principal" => "service_principal_name",
            other => {
                return Err(bad(format!(
                    "Permisos: «{}» no es un tipo válido.",
                    shown(other)
                )))
            }
        };
        let level = one_of(&r.opt("level").unwrap_or_default(), &LEVELS, "Permiso")?;
        if level == "IS_OWNER" && kind == "group" {
            return Err(bad(format!(
                "Permisos: el propietario es un usuario o un service principal, no un grupo («{}»).",
                shown(&who)
            )));
        }
        let e = json!({ key: who, "permission_level": level });
        if !out.contains(&e) {
            owners += usize::from(level == "IS_OWNER");
            out.push(e);
        }
    }
    if owners > 1 {
        return Err(bad("Permisos: el job tiene un solo propietario.".into()));
    }
    Ok(Some(out))
}

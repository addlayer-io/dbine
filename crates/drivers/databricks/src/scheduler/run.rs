//! "Ejecutar con parámetros": a run of the job now (`jobs/run-now`) with
//! other values for its parameters and, if the user wants, only some of its
//! tasks, as the workspace's "Run now with different settings" does.
//!
//! - A job with job parameters: a field per parameter, its default as the
//!   value, and new ones as `nombre=valor` lines; `job_parameters` carries
//!   only what differs from the defaults. The tasks receive them (the
//!   key-value ones by themselves, the rest through
//!   `{{job.parameters.<name>}}`), and the task-level fields aren't offered:
//!   the workspace changes only the job's at run time then.
//! - A job without them: the task-level parameters `run-now` takes for the
//!   kinds of task it has: notebooks' `notebook_params`, Python scripts'
//!   and positional wheels' `python_params`, wheels' `python_named_params`,
//!   SQL queries' and files' `sql_params`, JARs' `jar_params` and
//!   spark-submit's `spark_submit_params`. Each reaches every task of its
//!   kind, so its value is what those tasks share (a key or a list they
//!   don't agree on is left out) and it goes in the request only when the
//!   user changed it: notebooks get only the keys that changed (the API
//!   merges them over each notebook's base parameters), the rest the whole
//!   value. New job parameters can be added only to a job whose tasks take
//!   none of these, so the two never go together.
//! - Pipelines: a full refresh (`pipeline_params.full_refresh`), when it
//!   differs from what their tasks say.
//! - The tasks to run (`only`); none or all of them: the whole job.
//! - Every value is checked (the job's own names and keys, the API's rules
//!   for names, its 10,000-byte limit on task-level parameters and their
//!   ASCII-only argument lists) and goes into the request as JSON. The form
//!   carries the job's fingerprint: if the job changed since it was read,
//!   the run is refused. The script's "⚠" lines say when a value reaches
//!   several tasks and when a full refresh recomputes the pipelines' tables.

use super::lines::{parse_args, parse_map, render_args, render_map};
use super::plan::{json_lines, one_line, version};
use super::{bad, check_kind, shown, Current, Vals, CHANGED};
use crate::{jobs, DatabricksSession, Json};
use dbine_driver::{Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result};
use serde_json::{json, Map};
use std::collections::BTreeSet;

/// The action's key.
pub(crate) const ACTION: &str = "run_with_params";

/// The prefix of the job parameters' fields (`param:<name>`; a name has no
/// `:`).
const PARAM: &str = "param:";
/// The API's limit on each task-level parameter field, as JSON.
const MAX_BYTES: usize = 10_000;

const HEADER: [&str; 2] = [
    "Databricks Jobs API: la solicitud que DBine envía al ejecutar el job.",
    "El editor de SQL no la envía: va comentada.",
];
const JOB_PARAMS_NOTE: &str = "Con parámetros del job, el workspace cambia al ejecutarlo solo los del job, no los de cada tarea: las tareas los reciben (las de parámetros clave-valor, solas; las demás, con {{job.parameters.<nombre>}}).";
const TASK_PARAMS_NOTE: &str = "Cada parámetro de las tareas vale para todas las tareas de su tipo. Se muestra lo que esas tareas comparten; se envía solo lo que se cambie y, en los notebooks, cada uno conserva el resto de los suyos.";
const JOB_PARAMS_ONLY: &str =
    "Este job tiene parámetros del job: al ejecutarlo no se cambian los parámetros de sus tareas.";
const NO_NEW_PARAMS: &str = "Las tareas de este job reciben sus propios parámetros: al ejecutarlo no se le agregan parámetros del job.";
const NOTEBOOK_AND_JAR: &str = "Databricks no acepta parámetros de notebooks y de JAR en la misma ejecución: cambiá unos u otros.";
const NO_PIPELINES: &str =
    "El job no tiene tareas de pipeline: no hay nada que actualizar por completo.";

// What the script warns about ("⚠" lines: the app lists them and asks to
// continue), each a whole sentence of its own for the message catalog.

/// A task-level value reaches every task of its kind.
pub(super) const ALL_OF_A_KIND: &str = "Los parámetros de las tareas que se cambian valen para todas las tareas de su tipo del job, también para las que tenían otros.";
/// A full refresh of the job's pipelines.
pub(super) const FULL_REFRESH: &str = "La actualización completa vacía y vuelve a calcular las tablas de los pipelines del job: lo que sus fuentes ya no tengan se pierde.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `clave=valor` (a string map).
    Map,
    /// One argument per line (a string list).
    Args,
}

/// A task-level parameter field of `run-now`.
#[derive(Debug)]
struct Legacy {
    /// The request's field, which is the form's key too.
    key: &'static str,
    shape: Shape,
    /// The API takes only ASCII in it.
    ascii: bool,
}

const LEGACY: [Legacy; 6] = [
    Legacy {
        key: "notebook_params",
        shape: Shape::Map,
        ascii: false,
    },
    Legacy {
        key: "python_params",
        shape: Shape::Args,
        ascii: true,
    },
    Legacy {
        key: "python_named_params",
        shape: Shape::Map,
        ascii: false,
    },
    Legacy {
        key: "sql_params",
        shape: Shape::Map,
        ascii: false,
    },
    Legacy {
        key: "jar_params",
        shape: Shape::Args,
        ascii: false,
    },
    Legacy {
        key: "spark_submit_params",
        shape: Shape::Args,
        ascii: true,
    },
];

/// Whether `task` takes the task-level field `key` and, if so, its own
/// value for it (`None`: it has none).
fn takes<'a>(key: &str, task: &'a Json) -> Option<Option<&'a Json>> {
    let wheel = task.get("python_wheel_task");
    let named = wheel.is_some_and(|w| w.get("named_parameters").is_some());
    match key {
        "notebook_params" => task.get("notebook_task").map(|t| t.get("base_parameters")),
        "python_params" => task
            .get("spark_python_task")
            .or(wheel.filter(|_| !named))
            .map(|t| t.get("parameters")),
        "python_named_params" => wheel.filter(|_| named).map(|t| t.get("named_parameters")),
        "sql_params" => task
            .get("sql_task")
            .filter(|t| t.get("query").is_some() || t.get("file").is_some())
            .map(|t| t.get("parameters")),
        "jar_params" => task.get("spark_jar_task").map(|t| t.get("parameters")),
        "spark_submit_params" => task.get("spark_submit_task").map(|t| t.get("parameters")),
        _ => None,
    }
}

/// What the tasks' own values share: the keys every task that has them
/// agrees on, or the list when they're all the same (else none).
fn shared(shape: Shape, values: &[Option<&Json>]) -> Json {
    match shape {
        Shape::Map => {
            let maps: Vec<Map<String, Json>> = values
                .iter()
                .map(|v| v.and_then(Json::as_object).cloned().unwrap_or_default())
                .collect();
            let mut out = Map::new();
            let keys: BTreeSet<&String> = maps.iter().flat_map(|m| m.keys()).collect();
            for k in keys {
                let mut seen = maps.iter().filter_map(|m| m.get(k));
                let first = seen.next();
                if let Some(first) = first {
                    if seen.all(|v| v == first) {
                        out.insert(k.clone(), first.clone());
                    }
                }
            }
            Json::Object(out)
        }
        Shape::Args => {
            let lists: Vec<Vec<Json>> = values
                .iter()
                .map(|v| v.and_then(Json::as_array).cloned().unwrap_or_default())
                .collect();
            match lists.split_first() {
                Some((first, rest)) if rest.iter().all(|l| l == first) => json!(first),
                _ => json!([]),
            }
        }
    }
}

fn render(shape: Shape, v: &Json) -> String {
    match shape {
        Shape::Map => render_map(&v.as_object().cloned().unwrap_or_default()),
        Shape::Args => render_args(v.as_array().map_or(&[][..], Vec::as_slice)),
    }
}

/// What the form offers for a job.
#[derive(Debug)]
struct Offer {
    /// The job's parameters: name and default.
    declared: Vec<(String, String)>,
    /// The task-level fields its tasks take (only without job parameters):
    /// the field, what its tasks share, and how many tasks it reaches.
    legacy: Vec<(&'static Legacy, Json, usize)>,
    /// New job parameters may be added.
    new_params: bool,
    /// It has pipeline tasks: whether they all refresh fully now.
    full_refresh: Option<bool>,
    /// Its tasks' keys, in its order.
    tasks: Vec<String>,
}

fn offer(settings: &Json) -> Offer {
    let declared: Vec<(String, String)> = settings
        .get("parameters")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?.to_string();
            let default = match p.get("default") {
                Some(Json::String(s)) => s.clone(),
                None | Some(Json::Null) => String::new(),
                Some(v) => v.to_string(),
            };
            Some((name, default))
        })
        .collect();
    let tasks: Vec<&Json> = settings
        .get("tasks")
        .and_then(Json::as_array)
        .map(|t| t.iter().collect())
        .unwrap_or_default();
    let legacy: Vec<(&'static Legacy, Json, usize)> = if declared.is_empty() {
        LEGACY
            .iter()
            .filter_map(|l| {
                let own: Vec<Option<&Json>> =
                    tasks.iter().filter_map(|t| takes(l.key, t)).collect();
                (!own.is_empty()).then(|| (l, shared(l.shape, &own), own.len()))
            })
            .collect()
    } else {
        Vec::new()
    };
    let pipelines: Vec<bool> = tasks
        .iter()
        .filter_map(|t| t.get("pipeline_task"))
        .map(|p| p.get("full_refresh").and_then(Json::as_bool) == Some(true))
        .collect();
    Offer {
        new_params: !declared.is_empty() || legacy.is_empty(),
        declared,
        legacy,
        full_refresh: (!pipelines.is_empty()).then(|| pipelines.iter().all(|b| *b)),
        tasks: tasks
            .iter()
            .filter_map(|t| t.get("task_key").and_then(Json::as_str))
            .map(str::to_string)
            .collect(),
    }
}

/// A task-level field, its label in Spanish written here.
fn legacy_field(key: &str) -> FormField {
    const KEY_VALUE: &str = "fecha=2026-11-01";
    const ARGS: &str = "--fecha\n2026-11-01";
    match key {
        "notebook_params" => FormField::new(
            "notebook_params",
            "Parámetros de los notebooks",
            FormKind::Textarea,
        )
        .placeholder(KEY_VALUE)
        .help("Uno por línea, clave=valor. Se suman a los parámetros base de cada notebook y ganan sobre ellos; quitar una línea no quita el de la tarea."),
        "python_params" => FormField::new(
            "python_params",
            "Argumentos de los scripts de Python",
            FormKind::Textarea,
        )
        .placeholder(ARGS)
        .help("Uno por línea (o una lista JSON), solo caracteres ASCII. Reemplazan los de cada script de Python y de cada wheel sin argumentos con nombre."),
        "python_named_params" => FormField::new(
            "python_named_params",
            "Argumentos con nombre de los wheels",
            FormKind::Textarea,
        )
        .placeholder(KEY_VALUE)
        .help("Uno por línea, clave=valor, para cada tarea de wheel de Python con argumentos con nombre."),
        "sql_params" => FormField::new(
            "sql_params",
            "Parámetros de las consultas SQL",
            FormKind::Textarea,
        )
        .placeholder(KEY_VALUE)
        .help("Uno por línea, clave=valor, para cada tarea de consulta o archivo SQL."),
        "jar_params" => FormField::new("jar_params", "Argumentos de los JAR", FormKind::Textarea)
            .placeholder(ARGS)
            .help("Uno por línea (o una lista JSON). Reemplazan los de cada tarea de JAR."),
        _ => FormField::new(
            "spark_submit_params",
            "Argumentos de spark-submit",
            FormKind::Textarea,
        )
        .placeholder(ARGS)
        .help("Uno por línea (o una lista JSON), solo caracteres ASCII. Reemplazan los de cada tarea de spark-submit."),
    }
}

/// The action's form for the job.
pub(super) fn form_of(cur: &Current) -> ObjectForm {
    let o = offer(cur.settings());
    let mut f = ObjectForm::default();
    let mut sections = Vec::new();
    if !o.declared.is_empty() || o.new_params {
        let mut s = FormSection::new("job_parameters", "Parámetros del job");
        for (name, default) in &o.declared {
            let key = format!("{PARAM}{name}");
            f.values.insert(key.clone(), json!(default));
            s = s.field(FormField::new(&key, name, FormKind::Text));
        }
        if o.new_params {
            f.values.insert("new_parameters".into(), json!(""));
            s = s.field(
                FormField::new("new_parameters", "Parámetros nuevos", FormKind::Textarea)
                    .placeholder("fecha=2026-11-01")
                    .help("Uno por línea, nombre=valor: parámetros que el job no declara, solo para esta ejecución."),
            );
        }
        sections.push(s);
    }
    if !o.legacy.is_empty() {
        let mut s = FormSection::new("task_parameters", "Parámetros de las tareas");
        for (l, value, _) in &o.legacy {
            f.values.insert(l.key.into(), json!(render(l.shape, value)));
            s = s.field(legacy_field(l.key));
        }
        sections.push(s);
    }
    let mut run = FormSection::new("run", "Ejecución").field(
        FormField::new(
            "only",
            "Tareas",
            FormKind::MultiSelect {
                options: o.tasks.iter().map(|t| (t.clone(), t.clone())).collect(),
            },
        )
        .help("Vacío: todas. Solo las elegidas, sin las que dependen de ellas (para eso está «Iniciar en el paso…»)."),
    );
    f.values.insert("only".into(), json!([]));
    if let Some(full) = o.full_refresh {
        f.values.insert("full_refresh".into(), json!(full));
        run = run.field(
            FormField::new(
                "full_refresh",
                "Actualización completa de los pipelines",
                FormKind::Bool,
            )
            .help("Vacía y vuelve a calcular todas las tablas de los pipelines del job."),
        );
    }
    sections.push(run);
    f.sections = sections;
    f.values.insert("version".into(), json!(version(cur)));
    if !o.declared.is_empty() {
        f.notes.push(JOB_PARAMS_NOTE.into());
    }
    if !o.legacy.is_empty() {
        f.notes.push(TASK_PARAMS_NOTE.into());
    }
    f
}

/// A job parameter's name, as the API takes it.
fn name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 255
        && n.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

/// A text value as the app sent it: `None` when missing; a list or an
/// object isn't one.
fn text_of(v: Option<&Json>, label: &str) -> Result<Option<String>> {
    match v {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(Json::Number(n)) => Ok(Some(n.to_string())),
        Some(Json::Bool(b)) => Ok(Some(b.to_string())),
        Some(_) => Err(bad(format!("«{}» tiene que ser un texto.", shown(label)))),
    }
}

/// A scalar as the string the API takes.
fn string_of(x: Json) -> Json {
    match x {
        Json::String(s) => json!(s),
        x => json!(x.to_string()),
    }
}

/// A map's or a list's values as strings.
fn as_strings(v: Json) -> Json {
    match v {
        Json::Object(m) => Json::Object(m.into_iter().map(|(k, x)| (k, string_of(x))).collect()),
        Json::Array(a) => Json::Array(a.into_iter().map(string_of).collect()),
        v => v,
    }
}

/// The run: the request's body and the script's "⚠" lines.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Run {
    pub(super) body: Json,
    pub(super) warnings: Vec<String>,
    /// The job's name and id, for the script.
    name: String,
    id: u64,
}

impl Run {
    pub(super) fn script(&self) -> String {
        let mut out = String::new();
        for h in HEADER {
            out.push_str(&format!("-- {h}\n"));
        }
        for w in &self.warnings {
            out.push_str(&format!("-- ⚠ {}\n", one_line(w)));
        }
        out.push_str("--\n");
        let changes = self
            .body
            .as_object()
            .is_some_and(|b| b.keys().any(|k| k != "job_id"));
        let what = if changes {
            format!(
                "Ejecuta ahora el job «{}» ({}) con estos valores; lo demás, como está configurado.",
                self.name, self.id
            )
        } else {
            format!(
                "Ejecuta ahora el job «{}» ({}) como está configurado: no cambia ningún valor.",
                self.name, self.id
            )
        };
        out.push_str(&format!("-- {}\n", one_line(&what)));
        out.push_str("-- POST /api/2.1/jobs/run-now\n");
        for line in json_lines(&self.body).lines() {
            out.push_str(&format!("-- {line}\n"));
        }
        out
    }
}

/// The run the form's values ask for, checked against the job as it is.
pub(super) fn run_of(cur: &Current, values: &FormValues) -> Result<Run> {
    let v = Vals(&values.values);
    if let Some(seen) = v.opt("version") {
        if seen != version(cur) {
            return Err(bad(CHANGED.into()));
        }
    }
    let o = offer(cur.settings());
    let mut body = Map::new();
    body.insert("job_id".into(), json!(cur.id));
    let mut warnings = Vec::new();

    // Job parameters: those that differ from their defaults, and new ones.
    let declared: BTreeSet<&str> = o.declared.iter().map(|(n, _)| n.as_str()).collect();
    for k in values.values.keys() {
        if let Some(name) = k.strip_prefix(PARAM) {
            if !declared.contains(name) {
                return Err(bad(format!(
                    "«{}» no es un parámetro del job: recargá el formulario.",
                    shown(name)
                )));
            }
        }
    }
    let mut params = Map::new();
    for (name, default) in &o.declared {
        let key = format!("{PARAM}{name}");
        if let Some(value) = text_of(values.values.get(&key), name)? {
            if value != *default {
                params.insert(name.clone(), json!(value));
            }
        }
    }
    if let Some(new) = parse_map(&v.raw("new_parameters"), "Parámetros nuevos")? {
        if !o.new_params && !new.is_empty() {
            return Err(bad(NO_NEW_PARAMS.into()));
        }
        for (name, value) in new {
            if !name_ok(&name) {
                return Err(bad(format!(
                    "Parámetros nuevos: «{}» no es un nombre válido (letras, números, «_», «-» y «.»).",
                    shown(&name)
                )));
            }
            if declared.contains(name.as_str()) {
                return Err(bad(format!(
                    "Parámetros nuevos: «{}» ya es un parámetro del job: cambialo en su campo.",
                    shown(&name)
                )));
            }
            params.insert(name, string_of(value));
        }
    }
    if !params.is_empty() {
        body.insert("job_parameters".into(), Json::Object(params));
    }

    // Task-level parameters: the ones the user changed.
    for l in &LEGACY {
        let offered = o.legacy.iter().find(|(x, _, _)| x.key == l.key);
        let text = v.raw(l.key);
        let label = legacy_field(l.key).label;
        let Some((_, shared_value, reach)) = offered else {
            if !text.trim().is_empty() {
                return Err(bad(if o.declared.is_empty() {
                    format!("El job no tiene tareas que reciban «{label}».")
                } else {
                    JOB_PARAMS_ONLY.into()
                }));
            }
            continue;
        };
        let sent = match l.shape {
            Shape::Map => {
                let Some(m) = parse_map(&text, &label)? else {
                    continue;
                };
                let m = as_strings(Json::Object(m));
                let base = as_strings(shared_value.clone());
                if m == base {
                    continue;
                }
                if l.key == "notebook_params" {
                    // Merged over each notebook's own: only what changed.
                    let base = base.as_object().cloned().unwrap_or_default();
                    let changed: Map<String, Json> = m
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter(|(k, x)| base.get(*k) != Some(*x))
                        .map(|(k, x)| (k.clone(), x.clone()))
                        .collect();
                    if changed.is_empty() {
                        continue;
                    }
                    Json::Object(changed)
                } else {
                    m
                }
            }
            Shape::Args => {
                let Some(a) = parse_args(&text, &label)? else {
                    continue;
                };
                let a = as_strings(Json::Array(a));
                if a == as_strings(shared_value.clone()) {
                    continue;
                }
                if l.ascii
                    && a.as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Json::as_str)
                        .any(|s| !s.is_ascii())
                {
                    return Err(bad(format!(
                        "«{label}»: Databricks admite solo caracteres ASCII en estos argumentos."
                    )));
                }
                a
            }
        };
        let size = serde_json::to_string(&json!({ l.key: &sent })).map_or(0, |s| s.len());
        if size > MAX_BYTES {
            return Err(bad(format!(
                "«{label}» pasa de los {MAX_BYTES} bytes que admite Databricks."
            )));
        }
        if *reach > 1 && !warnings.iter().any(|w| w == ALL_OF_A_KIND) {
            warnings.push(ALL_OF_A_KIND.to_string());
        }
        body.insert(l.key.into(), sent);
    }
    if body.contains_key("notebook_params") && body.contains_key("jar_params") {
        return Err(bad(NOTEBOOK_AND_JAR.into()));
    }

    // Pipelines: a full refresh, when it isn't what their tasks say.
    let full = match values.values.get("full_refresh") {
        None | Some(Json::Null) => None,
        Some(Json::Bool(b)) => Some(*b),
        Some(_) => Some(v.bool("full_refresh")),
    };
    match (o.full_refresh, full) {
        (Some(now), Some(wanted)) if now != wanted => {
            body.insert("pipeline_params".into(), json!({ "full_refresh": wanted }));
            if wanted {
                warnings.push(FULL_REFRESH.to_string());
            }
        }
        (None, Some(true)) => return Err(bad(NO_PIPELINES.into())),
        _ => {}
    }

    // The tasks to run: some of them, or the whole job.
    let only = v.strings("only");
    for t in &only {
        if !o.tasks.contains(t) {
            return Err(bad(format!("«{}» no es una tarea del job.", shown(t))));
        }
    }
    if !only.is_empty() && only.len() < o.tasks.len() {
        // In the job's order.
        let chosen: Vec<&String> = o.tasks.iter().filter(|t| only.contains(t)).collect();
        body.insert("only".into(), json!(chosen));
    }

    Ok(Run {
        body: Json::Object(body),
        warnings,
        name: cur.name(),
        id: cur.id,
    })
}

/// The kind and the action are the ones this module serves.
fn check(kind: &str, action: &str) -> Result<()> {
    check_kind(kind)?;
    if action.trim() == ACTION {
        Ok(())
    } else {
        Err(Error::Unsupported(format!(
            "los jobs de Databricks no tienen la acción «{}»",
            shown(action)
        )))
    }
}

impl DatabricksSession {
    /// The job now, without its permissions (the run doesn't change them).
    async fn job_now(&self, id: &str) -> Result<Current> {
        let n = jobs::checked(id)?;
        let job = self.job_json(n).await?;
        if job.get("settings").is_none() {
            return Err(bad(format!("Databricks no devolvió el job {n}")));
        }
        Ok(Current {
            id: n,
            job,
            acl: None,
        })
    }

    pub(crate) async fn sched_action_form(
        &self,
        kind: &str,
        id: &str,
        action: &str,
    ) -> Result<ObjectForm> {
        check(kind, action)?;
        Ok(form_of(&self.job_now(id).await?))
    }

    pub(crate) async fn sched_action_script(
        &self,
        kind: &str,
        id: &str,
        action: &str,
        values: &FormValues,
    ) -> Result<String> {
        check(kind, action)?;
        Ok(run_of(&self.job_now(id).await?, values)?.script())
    }

    /// `run-now` with the form's values; answers once the run is queued.
    pub(crate) async fn sched_action(
        &self,
        kind: &str,
        id: &str,
        action: &str,
        values: &FormValues,
    ) -> Result<()> {
        check(kind, action)?;
        let run = run_of(&self.job_now(id).await?, values)?;
        self.api
            .post("/api/2.1/jobs/run-now", &run.body)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests;

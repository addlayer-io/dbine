//! The `job` kind: a task graph, as the jobs list shows it (its root, or a
//! standalone task, by the same id), with its child tasks and its finalizer
//! as the form's "Tareas del grafo" list.
//!
//! Snowflake changes a graph only with its root suspended (a standalone
//! task too): a change to a started one suspends the root first and resumes
//! it last; child tasks keep their own state, and new ones are created
//! suspended, so the script resumes the ones the form wants running. Tasks
//! can't be renamed. A graph that belongs to a task graph object is changed
//! without suspending it (the object stages the change) and committed when
//! the form says so; its schedule, overlap, configuration, retries,
//! notifications and finalizer belong to the object.

use super::{
    bad, checked_id, expression, held, ident, json_object, load, one_statement, params_list,
    params_of, parse_schema, relation_id, rows, same_json, schedule_fields, schedule_of,
    schedule_values, shown, shown_ident, together, trim_code, Choices, Kind, Plan, Run, Schedule,
    Vals, LOG_LEVELS, OVERLAPS, SIZES, TRACE_LEVELS,
};
use super::{completion_interval, log_select, overlap_select, size_key, size_select, trace_select};
use crate::jobs::{self, get, job_id, last_name, literal, relation, text};
use crate::monitor::{quote, Set};
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Task parameters with a field of their own (not in the session
/// parameters' list).
pub(crate) const OWN_PARAMS: [&str; 10] = [
    "USER_TASK_TIMEOUT_MS",
    "SUSPEND_TASK_AFTER_NUM_FAILURES",
    "TASK_AUTO_RETRY_ATTEMPTS",
    "USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE",
    "USER_TASK_MINIMUM_TRIGGER_INTERVAL_IN_SECONDS",
    "SERVERLESS_TASK_MIN_STATEMENT_SIZE",
    "SERVERLESS_TASK_MAX_STATEMENT_SIZE",
    "LOG_LEVEL",
    "TRACE_LEVEL",
    "TARGET_COMPLETION_INTERVAL",
];

/// A graph's tasks, at most (Snowflake's limit, root included).
const MAX_GRAPH: usize = 1000;
/// Predecessors of one task, at most.
const MAX_AFTER: usize = 100;

const RENAME: &str = "Snowflake no permite renombrar una tarea.";
const NOT_OWNER: &str = "Solo el rol propietario de la tarea (y los roles que lo heredan) puede cambiarla o borrarla: el rol de la sesión no lo tiene.";
const ORPHAN: &str =
    "Es una tarea hija cuya raíz el rol no ve: no se puede cambiar ni borrar desde aquí.";
const APP_OWNED: &str =
    "La tarea es de una aplicación nativa de Snowflake: no se puede cambiar ni borrar.";
/// The note of a change saved through the root's task graph object.
const STAGED: &str =
    "El grafo de tareas guarda los cambios como pendientes; el COMMIT los publica.";
const SUSPENDS: &str ="Snowflake cambia un grafo (o una tarea suelta) solo con la raíz suspendida: el script la suspende primero y la reanuda al final.";
const NEW_SUSPENDED: &str =
    "Snowflake crea las tareas suspendidas: el script reanuda las que el formulario deja activas.";

// ---------------------------------------------------------------------------
// What the server has
// ---------------------------------------------------------------------------

/// One row of `SHOW TASKS`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Shown {
    pub(crate) database: String,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) sf_id: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) owner_type: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) warehouse: Option<String>,
    pub(crate) schedule: Option<String>,
    pub(crate) target_completion: Option<String>,
    /// The names (last part) of the tasks it runs after.
    pub(crate) predecessors: Vec<String>,
    pub(crate) started: bool,
    pub(crate) definition: Option<String>,
    pub(crate) condition: Option<String>,
    pub(crate) overlap: Option<String>,
    pub(crate) error_integration: Option<String>,
    pub(crate) success_integration: Option<String>,
    pub(crate) config: Option<String>,
    pub(crate) execute_as: Option<String>,
    pub(crate) created_on: Option<String>,
    pub(crate) last_committed_on: Option<String>,
    pub(crate) last_suspended_on: Option<String>,
    pub(crate) last_suspended_reason: Option<String>,
    /// The root (name) it finalizes.
    pub(crate) finalizes: Option<String>,
    /// The task graph object (id) it finalizes.
    pub(crate) finalizes_graph: Option<String>,
    /// The task graph object (id) a root belongs to.
    pub(crate) graph: Option<String>,
}

impl Shown {
    pub(crate) fn id(&self) -> String {
        job_id(&self.database, &self.schema, &self.name)
    }

    pub(crate) fn is_root(&self) -> bool {
        self.predecessors.is_empty() && self.finalizes.is_none() && self.finalizes_graph.is_none()
    }

    fn app_owned(&self) -> bool {
        self.owner_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("APPLICATION"))
    }
}

/// `predecessors` of `SHOW TASKS`: a JSON array of qualified names.
fn predecessors(v: Option<&str>) -> Vec<String> {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return Vec::new();
    };
    let names: Vec<String> = serde_json::from_str(v).unwrap_or_else(|_| {
        v.trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .collect()
    });
    names
        .iter()
        .map(|n| last_name(n))
        .filter(|n| !n.is_empty())
        .collect()
}

pub(crate) fn shown_rows(s: &Set) -> Vec<Shown> {
    let mut all: Vec<Shown> = (0..s.rows.len())
        .filter_map(|i| {
            let rel = get(s, i, "task_relations");
            let overlap = text(s, i, "overlap_policy").or_else(|| {
                text(s, i, "allow_overlapping_execution").map(|v| {
                    if v.eq_ignore_ascii_case("true") {
                        "ALLOW_CHILD_OVERLAP".into()
                    } else {
                        "NO_OVERLAP".into()
                    }
                })
            });
            Some(Shown {
                database: text(s, i, "database_name")?,
                schema: text(s, i, "schema_name")?,
                name: text(s, i, "name")?,
                sf_id: text(s, i, "id"),
                owner: text(s, i, "owner"),
                owner_type: text(s, i, "owner_role_type"),
                comment: text(s, i, "comment"),
                warehouse: text(s, i, "warehouse"),
                schedule: text(s, i, "schedule"),
                target_completion: text(s, i, "target_completion_interval"),
                predecessors: predecessors(get(s, i, "predecessors")),
                started: get(s, i, "state").is_some_and(|v| v.eq_ignore_ascii_case("started")),
                definition: text(s, i, "definition"),
                condition: text(s, i, "condition"),
                overlap,
                error_integration: text(s, i, "error_integration"),
                success_integration: text(s, i, "success_integration"),
                config: text(s, i, "config"),
                execute_as: text(s, i, "execute_as_user"),
                created_on: jobs::shown_time(get(s, i, "created_on")),
                last_committed_on: jobs::shown_time(get(s, i, "last_committed_on")),
                last_suspended_on: jobs::shown_time(get(s, i, "last_suspended_on")),
                last_suspended_reason: text(s, i, "last_suspended_reason"),
                finalizes: relation(rel, "FinalizedRootTask").map(|n| last_name(&n)),
                finalizes_graph: relation(rel, "FinalizedGraph").and_then(|n| relation_id(&n)),
                graph: relation(rel, "TaskGraph").and_then(|n| relation_id(&n)),
            })
        })
        .collect();
    // A finalizer of a task graph object finalizes the graph's root.
    for i in 0..all.len() {
        if all[i].finalizes.is_none() {
            if let Some(g) = all[i].finalizes_graph.clone() {
                all[i].finalizes = all
                    .iter()
                    .find(|r| r.graph.as_deref() == Some(g.as_str()))
                    .map(|r| r.name.clone());
            }
        }
    }
    all
}

/// The members of a root's graph (children level by level, the finalizer
/// last), as indexes into `tasks`.
pub(crate) fn members_of(tasks: &[Shown], root: usize) -> Vec<usize> {
    let r = &tasks[root];
    let mut order = vec![root];
    let mut at = 0;
    while at < order.len() && order.len() < MAX_GRAPH {
        let parent = tasks[order[at]].name.clone();
        for (j, t) in tasks.iter().enumerate() {
            if !order.contains(&j) && t.predecessors.contains(&parent) && t.finalizes.is_none() {
                order.push(j);
            }
        }
        at += 1;
    }
    for (j, t) in tasks.iter().enumerate() {
        if t.finalizes.as_deref() == Some(r.name.as_str()) && !order.contains(&j) {
            order.push(j);
        }
    }
    order.remove(0);
    order
}

/// The root of a task (itself for a root), when the role sees it.
fn root_of(tasks: &[Shown], at: usize) -> Option<usize> {
    let mut cur = at;
    for _ in 0..MAX_GRAPH {
        let t = &tasks[cur];
        if t.is_root() {
            return Some(cur);
        }
        let up = t
            .finalizes
            .as_deref()
            .or(t.predecessors.first().map(String::as_str))?;
        cur = tasks.iter().position(|o| o.name == up)?;
    }
    None
}

/// A task's own parameters (`SHOW PARAMETERS IN TASK`, level `TASK`):
/// name → (value, type).
pub(crate) type Params = BTreeMap<String, (String, String)>;

fn params_rows(s: &Set) -> Params {
    (0..s.rows.len())
        .filter(|&i| get(s, i, "level").is_some_and(|l| l.eq_ignore_ascii_case("TASK")))
        .filter_map(|i| {
            Some((
                text(s, i, "key")?.to_ascii_uppercase(),
                (
                    get(s, i, "value").unwrap_or_default().to_string(),
                    text(s, i, "type").unwrap_or_default().to_ascii_uppercase(),
                ),
            ))
        })
        .collect()
}

/// What the server has of a graph: its root and members, with their own
/// parameters, and the schema's other tasks' names.
#[derive(Debug, Clone, Default)]
pub(crate) struct Graph {
    pub(crate) root: Shown,
    pub(crate) root_params: Params,
    pub(crate) members: Vec<(Shown, Params)>,
    /// Every task of the schema (a new task can't take one's name).
    pub(crate) schema_tasks: BTreeSet<String>,
    /// The role sees the task but not its root.
    pub(crate) orphan: bool,
}

pub(crate) async fn schema_tasks(r: Run<'_>, db: &str, schema: &str) -> Result<Vec<Shown>> {
    let s = r
        .rows(&format!(
            "SHOW TASKS IN SCHEMA {}.{}",
            quote(db),
            quote(schema)
        ))
        .await?;
    Ok(shown_rows(&s))
}

async fn params_of_tasks(r: Run<'_>, ids: &[String]) -> Vec<Params> {
    let sqls: Vec<String> = ids
        .iter()
        .map(|id| format!("SHOW PARAMETERS IN TASK {id}"))
        .collect();
    together(sqls.iter().map(|q| r.rows(q)).collect())
        .await
        .into_iter()
        .map(|a| match a {
            Ok(s) => params_rows(&s),
            Err(e) => {
                tracing::debug!("snowflake scheduler: task parameters not read: {e}");
                Params::new()
            }
        })
        .collect()
}

fn not_found(db: &str, schema: &str, name: &str) -> Error {
    bad(format!(
        "No existe la tarea {db}.{schema}.{name} o el rol no la ve."
    ))
}

pub(crate) async fn read(r: Run<'_>, id: &str) -> Result<Graph> {
    let (db, schema, name) = checked_id(Kind::Job, id)?;
    let tasks = schema_tasks(r, &db, &schema).await?;
    let at = tasks
        .iter()
        .position(|t| t.name == name)
        .ok_or_else(|| not_found(&db, &schema, &name))?;
    let mut g = Graph {
        schema_tasks: tasks.iter().map(|t| t.name.clone()).collect(),
        ..Graph::default()
    };
    let members: Vec<usize> = if tasks[at].is_root() {
        members_of(&tasks, at)
    } else if let Some(root) = root_of(&tasks, at) {
        return Err(bad(format!(
            "La tarea «{}» es parte del grafo de «{}»: se cambia desde ese grafo.",
            tasks[at].name, tasks[root].name
        )));
    } else {
        g.orphan = true;
        Vec::new()
    };
    let mut ids = vec![tasks[at].id()];
    ids.extend(members.iter().map(|&i| tasks[i].id()));
    let mut params = params_of_tasks(r, &ids).await.into_iter();
    g.root = tasks[at].clone();
    g.root_params = params.next().unwrap_or_default();
    g.members = members
        .iter()
        .map(|&i| (tasks[i].clone(), params.next().unwrap_or_default()))
        .collect();
    Ok(g)
}

// ---------------------------------------------------------------------------
// What a task is: the same shape for the server's and the form's
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Props {
    /// `None`: serverless.
    pub(crate) warehouse: Option<String>,
    pub(crate) initial_size: Option<String>,
    pub(crate) min_size: Option<String>,
    pub(crate) max_size: Option<String>,
    pub(crate) target_completion: Option<String>,
    pub(crate) timeout_ms: Option<i64>,
    pub(crate) execute_as: Option<String>,
    pub(crate) log_level: Option<String>,
    pub(crate) trace_level: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) condition: Option<String>,
    pub(crate) definition: String,
    /// Session parameters: `(NAME, value as written)`.
    pub(crate) params: Vec<(String, String)>,
    pub(crate) enabled: bool,
}

/// What only a root (or a standalone task) has.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RootProps {
    pub(crate) schedule: Option<Schedule>,
    pub(crate) overlap: String,
    pub(crate) config: Option<String>,
    pub(crate) auto_retry: Option<i64>,
    pub(crate) suspend_after: Option<i64>,
    pub(crate) min_trigger: Option<i64>,
    pub(crate) error_integration: Option<String>,
    pub(crate) success_integration: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Role {
    /// Runs after these tasks (names; the root's among them).
    Child(Vec<String>),
    Finalizer,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Member {
    /// Its name on the server (a row read from it).
    pub(crate) original: Option<String>,
    pub(crate) name: String,
    pub(crate) role: Role,
    pub(crate) props: Props,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Spec {
    pub(crate) database: String,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) props: Props,
    pub(crate) root: RootProps,
    pub(crate) members: Vec<Member>,
    /// Commit the task graph object the root belongs to.
    pub(crate) commit: bool,
}

impl Spec {
    pub(crate) fn id(&self) -> String {
        job_id(&self.database, &self.schema, &self.name)
    }

    fn task_id(&self, name: &str) -> String {
        job_id(&self.database, &self.schema, name)
    }
}

fn param(p: &Params, k: &str) -> Option<String> {
    p.get(k)
        .map(|(v, _)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn param_int(p: &Params, k: &str) -> Option<i64> {
    param(p, k)
        .and_then(|v| v.parse::<f64>().ok())
        .map(|f| f as i64)
}

/// A server task's properties.
pub(crate) fn props_of(t: &Shown, p: &Params) -> Props {
    let mut params: Vec<(String, String)> = p
        .iter()
        .filter(|(k, _)| !OWN_PARAMS.contains(&k.as_str()))
        .map(|(k, (v, ty))| {
            let shown_v = super::param_value(k, v, Some(ty.as_str()).filter(|t| !t.is_empty()))
                .unwrap_or_else(|_| literal(v));
            (k.clone(), shown_v)
        })
        .collect();
    params.sort();
    Props {
        warehouse: t.warehouse.clone(),
        initial_size: param(p, "USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE").map(|v| size_key(&v)),
        min_size: param(p, "SERVERLESS_TASK_MIN_STATEMENT_SIZE").map(|v| size_key(&v)),
        max_size: param(p, "SERVERLESS_TASK_MAX_STATEMENT_SIZE").map(|v| size_key(&v)),
        target_completion: t
            .target_completion
            .clone()
            .or_else(|| param(p, "TARGET_COMPLETION_INTERVAL"))
            .and_then(|v| completion_interval(&v, "").ok().flatten().or(Some(v))),
        timeout_ms: param_int(p, "USER_TASK_TIMEOUT_MS"),
        execute_as: t.execute_as.clone(),
        log_level: param(p, "LOG_LEVEL").map(|v| v.to_ascii_uppercase()),
        trace_level: param(p, "TRACE_LEVEL").map(|v| v.to_ascii_uppercase()),
        comment: t.comment.clone(),
        condition: t
            .condition
            .as_deref()
            .map(trim_code)
            .filter(|c| !c.is_empty()),
        definition: trim_code(t.definition.as_deref().unwrap_or_default()),
        params,
        enabled: t.started,
    }
}

pub(crate) fn root_props_of(t: &Shown, p: &Params) -> RootProps {
    RootProps {
        schedule: t.schedule.as_deref().and_then(Schedule::parse),
        overlap: t.overlap.clone().unwrap_or_else(|| "NO_OVERLAP".into()),
        config: t.config.clone(),
        auto_retry: param_int(p, "TASK_AUTO_RETRY_ATTEMPTS"),
        suspend_after: param_int(p, "SUSPEND_TASK_AFTER_NUM_FAILURES"),
        min_trigger: param_int(p, "USER_TASK_MINIMUM_TRIGGER_INTERVAL_IN_SECONDS"),
        error_integration: t.error_integration.clone(),
        success_integration: t.success_integration.clone(),
    }
}

/// The server's graph as a spec.
pub(crate) fn current(g: &Graph) -> Spec {
    let root = &g.root;
    Spec {
        database: root.database.clone(),
        schema: root.schema.clone(),
        name: root.name.clone(),
        props: props_of(root, &g.root_params),
        root: root_props_of(root, &g.root_params),
        members: g
            .members
            .iter()
            .map(|(t, p)| Member {
                original: Some(t.name.clone()),
                name: t.name.clone(),
                role: if t.finalizes.is_some() {
                    Role::Finalizer
                } else {
                    Role::Child(t.predecessors.clone())
                },
                props: props_of(t, p),
            })
            .collect(),
        commit: false,
    }
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

/// The jobs list's graphs, each with what the role may do with it, as its
/// form and its delete say: a graph another role owns (and the role's
/// roles don't inherit), a native application's or a child whose root the
/// role doesn't see can't be changed nor dropped; a task graph object's
/// root is dropped with its graph, not alone.
pub(crate) async fn items(r: Run<'_>) -> Result<SchedulerList> {
    let (list, owned) = jobs::listed_owned(r).await?;
    // Only the rows the role check decides: a native application's owner
    // is the app, not a role, and an orphan child is decided already.
    let owners: Vec<String> = owned
        .iter()
        .filter(|o| !o.app && !o.orphan)
        .filter_map(|o| o.owner.clone())
        .collect();
    let roles = held(r, &owners).await;
    Ok(SchedulerList {
        items: list
            .jobs
            .iter()
            .zip(&owned)
            .map(|(j, o)| {
                let mine = !o.orphan
                    && !o.app
                    && o.owner
                        .as_ref()
                        .is_none_or(|w| roles.get(w).copied().unwrap_or(true));
                SchedulerItem {
                    id: j.id.clone(),
                    name: j.name.clone(),
                    description: j.description.clone(),
                    enabled: Some(j.enabled),
                    system: o.app,
                    can_edit: (!mine).then_some(false),
                    can_delete: (!mine || o.in_graph).then_some(false),
                    actions: None,
                    // A graph starts at its root (or retries from a task); a
                    // child whose root the role doesn't see runs only with
                    // its graph.
                    run_from_step: o.orphan.then_some(false),
                    clear_history: None,
                }
            })
            .collect(),
        notes: list.notes,
    })
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn compute_select() -> FormKind {
    FormKind::select(&[
        ("warehouse", "Warehouse"),
        (
            "serverless",
            "Serverless (cómputo administrado por Snowflake)",
        ),
    ])
}

/// The fields every task has (the root's form and the members' rows).
fn task_fields(row: bool) -> Vec<FormField> {
    let mut f = vec![
        FormField::new("compute", "Cómputo", compute_select()).default(json!("serverless")),
        FormField::new("warehouse", "Warehouse", FormKind::reference("warehouse", false))
            .when("compute", &["warehouse"]),
        FormField::new("initial_size", "Tamaño inicial", size_select())
            .when("compute", &["serverless"])
            .help("USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE: el tamaño de las primeras ejecuciones, hasta que Snowflake aprende el que necesita. Vacío: MEDIUM."),
        FormField::new("min_size", "Tamaño mínimo", size_select())
            .when("compute", &["serverless"]),
        FormField::new("max_size", "Tamaño máximo", size_select())
            .when("compute", &["serverless"]),
        FormField::new("target_completion", "Tiempo objetivo de finalización", FormKind::Text)
            .when("compute", &["serverless"])
            .placeholder("10 MINUTES")
            .help("TARGET_COMPLETION_INTERVAL: Snowflake dimensiona el cómputo para terminar en ese tiempo (de 10 segundos a 24 horas). Lo exigen las tareas serverless disparadas por datos."),
        FormField::new(
            "timeout_ms",
            "Tiempo máximo de ejecución (ms)",
            FormKind::number(Some(0.0), Some(604_800_000.0)),
        )
        .help("USER_TASK_TIMEOUT_MS. Vacío: el heredado (por defecto 3600000, una hora)."),
        FormField::new("execute_as", "Ejecutar como usuario", FormKind::Text)
            .help("EXECUTE AS USER: el usuario con cuyos privilegios corre la tarea; el rol necesita IMPERSONATE sobre él. Vacío: el usuario del sistema con el rol propietario."),
        FormField::new("log_level", "Nivel de registro", log_select()),
        FormField::new("trace_level", "Nivel de trazas", trace_select()),
    ];
    if row {
        f.push(
            FormField::new("session", "Parámetros de sesión", FormKind::Textarea)
                .placeholder("TIMEZONE = 'UTC'")
                .help("Uno por línea: NOMBRE = valor."),
        );
    }
    f
}

/// The members' rows: their fields.
fn member_fields() -> Vec<FormField> {
    let mut f = vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help("Sin comillas, Snowflake lo guarda en mayúsculas; entre comillas dobles, tal cual. Snowflake no permite renombrar una tarea."),
        FormField::new(
            "role",
            "Tipo",
            FormKind::select(&[
                ("child", "Tarea hija"),
                ("finalizer", "Finalizador (corre al terminar el grafo)"),
            ]),
        )
        .default(json!("child")),
        FormField::new("after_root", "Después de la raíz", FormKind::Bool)
            .when("role", &["child"])
            .default(json!(true)),
        FormField::new("after", "Después de", FormKind::reference("list:tasks", true))
            .when("role", &["child"])
            .help("Corre cuando terminan bien todas estas tareas (hasta 100)."),
        FormField::new("condition", "Condición (WHEN)", FormKind::code("sql")).help(
            "Si da falso, la tarea se saltea: SYSTEM$GET_PREDECESSOR_RETURN_VALUE, SYSTEM$PREDECESSORS_STATE, SYSTEM$TASK_COMPLETION_STATE…",
        ),
        FormField::new("definition", "SQL", FormKind::code("sql"))
            .required()
            .help("Una instrucción, una llamada a un procedimiento o un bloque BEGIN … END."),
    ];
    f.extend(task_fields(true));
    f.push(FormField::new("comment", "Comentario", FormKind::Textarea));
    f.push(FormField::new("enabled", "Activa", FormKind::Bool).default(json!(true)));
    f
}

fn opt_json(v: &Option<String>) -> Value {
    json!(v.clone().unwrap_or_default())
}

fn put_props(v: &mut BTreeMap<String, Value>, p: &Props) {
    v.insert(
        "compute".into(),
        json!(if p.warehouse.is_some() {
            "warehouse"
        } else {
            "serverless"
        }),
    );
    v.insert("warehouse".into(), opt_json(&p.warehouse));
    v.insert("initial_size".into(), opt_json(&p.initial_size));
    v.insert("min_size".into(), opt_json(&p.min_size));
    v.insert("max_size".into(), opt_json(&p.max_size));
    v.insert("target_completion".into(), opt_json(&p.target_completion));
    v.insert(
        "timeout_ms".into(),
        p.timeout_ms.map_or(json!(""), |n| json!(n)),
    );
    v.insert(
        "execute_as".into(),
        json!(p.execute_as.as_deref().map(shown_ident).unwrap_or_default()),
    );
    v.insert("log_level".into(), opt_json(&p.log_level));
    v.insert("trace_level".into(), opt_json(&p.trace_level));
    v.insert("comment".into(), opt_json(&p.comment));
    v.insert("condition".into(), opt_json(&p.condition));
    v.insert("definition".into(), json!(p.definition));
    v.insert("enabled".into(), json!(p.enabled));
}

/// A member's row, its server name kept as `task`.
fn member_row(m: &Member, root: &str) -> BTreeMap<String, Value> {
    let mut v = BTreeMap::new();
    if let Some(o) = &m.original {
        v.insert("task".into(), json!(o));
    }
    v.insert("name".into(), json!(shown_ident(&m.name)));
    put_props(&mut v, &m.props);
    match &m.role {
        Role::Finalizer => {
            v.insert("role".into(), json!("finalizer"));
            v.insert("after_root".into(), json!(false));
            v.insert("after".into(), json!([]));
        }
        Role::Child(after) => {
            v.insert("role".into(), json!("child"));
            v.insert("after_root".into(), json!(after.iter().any(|a| a == root)));
            let others: Vec<String> = after
                .iter()
                .filter(|a| *a != root)
                .map(|a| shown_ident(a))
                .collect();
            v.insert("after".into(), json!(others));
        }
    }
    let session: Vec<String> = m
        .props
        .params
        .iter()
        .map(|(k, val)| format!("{k} = {val}"))
        .collect();
    v.insert("session".into(), json!(session.join("\n")));
    v
}

fn when_text(t: &Option<String>) -> Value {
    json!(t.clone().unwrap_or_default())
}

pub(crate) async fn form(r: Run<'_>, id: Option<&str>) -> Result<ObjectForm> {
    let mut sources = vec!["warehouse", "notification_integration", "session_parameter"];
    if id.is_none() {
        sources.push("schema");
    }
    let (graph, mut choices) = match id {
        Some(id) => {
            let (g, c) = tokio::join!(read(r, id), load(r, &sources));
            (Some(g?), c)
        }
        None => (None, load(r, &sources).await),
    };
    let mut f = ObjectForm::default();
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    let mut member_rows = Vec::new();
    let mut param_rows = Vec::new();
    let mut existing = false;
    let mut in_object: Option<String> = None;
    match &graph {
        None => {
            values.insert("name".into(), json!(""));
            let wh = r.warehouse();
            values.insert(
                "compute".into(),
                json!(if wh.is_some() {
                    "warehouse"
                } else {
                    "serverless"
                }),
            );
            values.insert("warehouse".into(), json!(wh.clone().unwrap_or_default()));
            if let Some(w) = &wh {
                choices.keep("warehouse", w, w);
            }
            values.insert("enabled".into(), json!(false));
            values.insert("overlap".into(), json!("NO_OVERLAP"));
            values.insert("commit".into(), json!(true));
            schedule_values(
                Some(&Schedule::Every(60, super::Unit::Minutes)),
                "none",
                &mut values,
            );
        }
        Some(g) => {
            existing = true;
            let spec = current(g);
            let t = &g.root;
            values.insert("name".into(), json!(shown_ident(&t.name)));
            let sid = super::schema_id(&t.database, &t.schema);
            choices.keep("schema", &sid, &format!("{}.{}", t.database, t.schema));
            values.insert("schema".into(), json!(sid));
            put_props(&mut values, &spec.props);
            if let Some(w) = &spec.props.warehouse {
                choices.keep("warehouse", w, w);
            }
            schedule_values(spec.root.schedule.as_ref(), "none", &mut values);
            if spec.root.schedule.is_none() {
                if let Some(raw) = &t.schedule {
                    // A schedule DBine doesn't read: shown as is, not changed.
                    f.notes.push(format!(
                        "Snowflake devolvió una programación que DBine no interpreta («{raw}»): si no la cambiás, se mantiene."
                    ));
                }
            }
            values.insert("overlap".into(), json!(spec.root.overlap));
            values.insert("config".into(), opt_json(&spec.root.config));
            values.insert(
                "auto_retry".into(),
                spec.root.auto_retry.map_or(json!(""), |n| json!(n)),
            );
            values.insert(
                "suspend_after".into(),
                spec.root.suspend_after.map_or(json!(""), |n| json!(n)),
            );
            values.insert(
                "min_trigger".into(),
                spec.root.min_trigger.map_or(json!(""), |n| json!(n)),
            );
            for (k, v) in [
                ("error_integration", &spec.root.error_integration),
                ("success_integration", &spec.root.success_integration),
            ] {
                values.insert(k.into(), opt_json(v));
                if let Some(n) = v {
                    choices.keep("notification_integration", n, n);
                }
            }
            values.insert("owner".into(), when_text(&t.owner));
            values.insert("created".into(), when_text(&t.created_on));
            values.insert("last_committed".into(), when_text(&t.last_committed_on));
            values.insert("last_suspended".into(), when_text(&t.last_suspended_on));
            values.insert(
                "suspended_reason".into(),
                when_text(&t.last_suspended_reason),
            );
            values.insert("sf_id".into(), when_text(&t.sf_id));
            values.insert("commit".into(), json!(true));
            if let Some(g) = &t.graph {
                values.insert("task_graph".into(), json!(g));
                in_object = Some(g.clone());
            }
            for (k, v) in &spec.props.params {
                param_rows.push(BTreeMap::from([
                    ("name".to_string(), json!(k)),
                    ("value".to_string(), json!(param_display(v))),
                ]));
                choices.keep("session_parameter", k, k);
            }
            for m in &spec.members {
                member_rows.push(member_row(m, &t.name));
                if let Some(w) = &m.props.warehouse {
                    choices.keep("warehouse", w, w);
                }
            }
        }
    }
    let obj = in_object.is_some();
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing)
            .help("Sin comillas, Snowflake lo guarda en mayúsculas; entre comillas dobles, tal cual. Snowflake no permite renombrar una tarea."),
        FormField::new("schema", "Esquema", FormKind::reference("schema", false))
            .required()
            .read_only(existing),
        FormField::new("comment", "Comentario", FormKind::Textarea),
        FormField::new("enabled", "Activa (RESUME)", FormKind::Bool)
            .read_only(obj)
            .default(json!(false))
            .help("Snowflake crea las tareas suspendidas; activa la reanuda al guardar y corre según su programación o su condición."),
    ]);
    if existing {
        general = general.fields(vec![
            FormField::new("owner", "Rol propietario", FormKind::Text).read_only(true),
            FormField::new("created", "Creada", FormKind::Text).read_only(true),
            FormField::new("last_committed", "Última versión", FormKind::Text).read_only(true),
            FormField::new("last_suspended", "Última suspensión", FormKind::Text).read_only(true),
            FormField::new(
                "suspended_reason",
                "Motivo de la suspensión",
                FormKind::Text,
            )
            .read_only(true),
            FormField::new("sf_id", "Id de Snowflake", FormKind::Text).read_only(true),
        ]);
    }
    if obj {
        general = general.fields(vec![
            FormField::new("task_graph", "Grafo de tareas (objeto)", FormKind::Text).read_only(true),
            FormField::new("commit", "Publicar los cambios del grafo (COMMIT)", FormKind::Bool)
                .default(json!(true))
                .help("Los cambios de un grafo de tareas quedan pendientes hasta el COMMIT, que también publica los que otros dejaron pendientes."),
        ]);
    }
    let mut sched = vec![FormField::new(
        "schedule",
        "Programación",
        FormKind::select(&[
            ("interval", "Cada cierto intervalo"),
            ("cron", "Expresión cron"),
            (
                "none",
                "Sin programación (por su condición o con EXECUTE TASK)",
            ),
        ]),
    )
    .read_only(obj)
    .default(json!("interval"))];
    sched.extend(schedule_fields(true).into_iter().map(|f| f.read_only(obj)));
    sched.push(
        FormField::new("condition", "Condición (WHEN)", FormKind::code("sql")).help(
            "Sin programación, una tarea con condición corre cuando hay datos nuevos: SYSTEM$STREAM_HAS_DATA('…'). Con programación, si da falso se saltea esa ejecución.",
        ),
    );
    sched.push(
        FormField::new(
            "min_trigger",
            "Intervalo mínimo entre disparos (segundos)",
            FormKind::number(Some(10.0), Some(604_800.0)),
        )
        .read_only(obj)
        // A triggered task is one without a schedule (it runs on its
        // streams' new data).
        .when("schedule", &["none"])
        .help("USER_TASK_MINIMUM_TRIGGER_INTERVAL_IN_SECONDS, para las tareas disparadas por datos. Vacío: el heredado (30)."),
    );
    let schedule = FormSection::new("schedule", "Programación").fields(sched);
    let compute = FormSection::new("compute", "Cómputo").fields(task_fields(false));
    let definition = FormSection::new("definition", "SQL").field(
        FormField::new(
            "definition",
            "SQL que ejecuta la tarea",
            FormKind::code("sql"),
        )
        .required()
        .help("Una instrucción, una llamada a un procedimiento o un bloque BEGIN … END."),
    );
    let graph_section = FormSection::new("graph", "Grafo")
        .fields(vec![
            FormField::new("overlap", "Ejecuciones superpuestas", overlap_select())
                .read_only(obj)
                .default(json!("NO_OVERLAP")),
            FormField::new("config", "Configuración (CONFIG)", FormKind::code("json"))
                .read_only(obj)
                .help("Un objeto JSON que las tareas leen con SYSTEM$GET_TASK_GRAPH_CONFIG."),
            FormField::new(
                "auto_retry",
                "Reintentos automáticos del grafo",
                FormKind::number(Some(0.0), Some(30.0)),
            )
            .read_only(obj)
            .help("TASK_AUTO_RETRY_ATTEMPTS: reintenta un grafo que falló desde la tarea que falló. Vacío: el heredado (0)."),
            FormField::new(
                "suspend_after",
                "Suspender tras fallos seguidos",
                FormKind::number(Some(0.0), None),
            )
            .read_only(obj)
            .help("SUSPEND_TASK_AFTER_NUM_FAILURES; 0 no suspende. Vacío: el heredado (10)."),
        ])
        .list(
            FormList::new("tasks", "Tareas del grafo", "name", member_fields())
                .rows(member_rows),
        );
    let notifications = FormSection::new("notifications", "Notificaciones").fields(vec![
        FormField::new(
            "error_integration",
            "Integración para los errores",
            FormKind::reference("notification_integration", false),
        )
        .read_only(obj)
        .help("Una integración de notificaciones (SNS, Event Grid o Pub/Sub) que avisa cuando falla una ejecución."),
        FormField::new(
            "success_integration",
            "Integración para los éxitos",
            FormKind::reference("notification_integration", false),
        )
        .read_only(obj),
    ]);
    let session =
        FormSection::new("session", "Parámetros de sesión").list(params_list(param_rows, false));
    f.sections = vec![
        general,
        schedule,
        definition,
        compute,
        graph_section,
        notifications,
        session,
    ];
    f.values = values;
    f.choices = choices.lists;
    if let Some(g) = &graph {
        if let Some(o) = &in_object {
            f.notes.push(format!(
                "La tarea es la raíz del grafo de tareas {o}: su programación, superposición, configuración, reintentos, suspensión por fallos, notificaciones, estado y finalizador se cambian en el grafo."
            ));
        } else if g.root.started {
            f.notes.push(SUSPENDS.into());
        }
        if g.orphan {
            f.read_only = true;
            f.notes.insert(0, ORPHAN.into());
        } else if g.root.app_owned() {
            f.read_only = true;
            f.notes.insert(0, APP_OWNED.into());
        } else if let Some(owner) = &g.root.owner {
            let roles = held(r, std::slice::from_ref(owner)).await;
            if !roles.get(owner).copied().unwrap_or(true) {
                f.read_only = true;
                f.notes.insert(0, NOT_OWNER.into());
            }
        }
    } else {
        f.notes.push(NEW_SUSPENDED.into());
    }
    if choices.failed.contains("notification_integration") {
        f.notes.push("El rol no puede listar las integraciones de notificaciones: se acepta el nombre que se escriba.".into());
    }
    Ok(f)
}

/// A parameter's value as the list shows it: a literal's text without its
/// quotes.
fn param_display(v: &str) -> String {
    super::unliteral(v)
}

// ---------------------------------------------------------------------------
// The form back into a spec
// ---------------------------------------------------------------------------

fn level(v: Vals, k: &str, label: &str, options: &[&'static str]) -> Result<Option<String>> {
    Ok(v.pick(k, label, options, None)?.map(str::to_string))
}

/// A task's properties from its values (the form's or a row's).
fn props_from(
    v: Vals,
    choices: &Choices,
    params: Vec<(String, String)>,
    label: &str,
) -> Result<Props> {
    let serverless = match v.text("compute").as_str() {
        "" | "serverless" => true,
        "warehouse" => false,
        other => return Err(bad(format!("«{}» no es un tipo de cómputo.", shown(other)))),
    };
    let warehouse = if serverless {
        None
    } else {
        let w = v
            .opt("warehouse")
            .ok_or_else(|| bad(format!("{label}: falta el warehouse.")))?;
        Some(choices.pick("warehouse", &w, "Warehouse")?)
    };
    let size = |k: &str, l: &str| -> Result<Option<String>> {
        if serverless {
            level(v, k, l, &SIZES)
        } else {
            Ok(None)
        }
    };
    let execute_as = match v.opt("execute_as") {
        Some(u) => Some(ident(&u, "Ejecutar como usuario")?),
        None => None,
    };
    let condition = expression(&v.raw("condition"), "Condición (WHEN)")?;
    let definition = one_statement(&v.raw("definition"), &format!("{label}: SQL"))?;
    Ok(Props {
        initial_size: size("initial_size", "Tamaño inicial")?,
        min_size: size("min_size", "Tamaño mínimo")?,
        max_size: size("max_size", "Tamaño máximo")?,
        target_completion: if serverless {
            completion_interval(
                &v.text("target_completion"),
                "Tiempo objetivo de finalización",
            )?
        } else {
            None
        },
        warehouse,
        timeout_ms: v.int(
            "timeout_ms",
            "Tiempo máximo de ejecución (ms)",
            0,
            604_800_000,
        )?,
        execute_as,
        log_level: level(v, "log_level", "Nivel de registro", &LOG_LEVELS)?,
        trace_level: level(v, "trace_level", "Nivel de trazas", &TRACE_LEVELS)?,
        comment: v.opt("comment"),
        condition,
        definition,
        params,
        enabled: v.bool("enabled"),
    })
    .and_then(|p| {
        let rank = |s: &Option<String>| {
            s.as_deref()
                .and_then(|s| SIZES.iter().position(|x| *x == s))
        };
        if let (Some(a), Some(b)) = (rank(&p.min_size), rank(&p.max_size)) {
            if a > b {
                return Err(bad(format!(
                    "{label}: el tamaño mínimo no puede ser mayor que el máximo."
                )));
            }
        }
        if let (Some(a), Some(b)) = (rank(&p.min_size), rank(&p.initial_size)) {
            if a > b {
                return Err(bad(format!(
                    "{label}: el tamaño mínimo no puede ser mayor que el inicial."
                )));
            }
        }
        if let (Some(a), Some(b)) = (rank(&p.initial_size), rank(&p.max_size)) {
            if a > b {
                return Err(bad(format!(
                    "{label}: el tamaño inicial no puede ser mayor que el máximo."
                )));
            }
        }
        Ok(p)
    })
}

/// The parameter types the server reported for these tasks.
fn known_types(g: Option<&Graph>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(g) = g {
        for p in std::iter::once(&g.root_params).chain(g.members.iter().map(|(_, p)| p)) {
            for (k, (_, t)) in p {
                if !t.is_empty() {
                    out.insert(k.clone(), t.clone());
                }
            }
        }
    }
    out
}

/// The form's values as a spec, checked. `g`: what the server has (an
/// edit).
pub(crate) fn spec(values: &FormValues, choices: &Choices, g: Option<&Graph>) -> Result<Spec> {
    let v = Vals(&values.values);
    let known = known_types(g);
    let (database, schema, name) = match g {
        Some(g) => (
            g.root.database.clone(),
            g.root.schema.clone(),
            g.root.name.clone(),
        ),
        None => {
            let sch = v
                .opt("schema")
                .ok_or_else(|| bad("Elegí el esquema de la tarea.".into()))?;
            let (db, s) = parse_schema(&sch)
                .ok_or_else(|| bad(format!("«{}» no es un esquema de Snowflake.", shown(&sch))))?;
            (db, s, ident(&v.text("name"), "Nombre")?)
        }
    };
    if let Some(g) = g {
        if !v.text("name").is_empty() && ident(&v.text("name"), "Nombre")? != g.root.name {
            return Err(bad(RENAME.into()));
        }
    }
    let in_object = g.and_then(|g| g.root.graph.clone());
    let params = params_of(values, choices, &known)?;
    let props = props_from(v, choices, params, "La tarea")?;
    let root = match (&in_object, g) {
        // The object owns these: the root keeps what it has.
        (Some(_), Some(g)) => root_props_of(&g.root, &g.root_params),
        _ => {
            let pick_int = |name: &str, l: &str, min, max| v.int(name, l, min, max);
            let integration = |k: &str, l: &str| -> Result<Option<String>> {
                match v.opt(k) {
                    Some(n) => Ok(Some(choices.pick("notification_integration", &n, l)?)),
                    None => Ok(None),
                }
            };
            RootProps {
                schedule: schedule_of(v, false)?,
                overlap: v
                    .pick(
                        "overlap",
                        "Ejecuciones superpuestas",
                        &OVERLAPS,
                        Some("NO_OVERLAP"),
                    )?
                    .unwrap_or("NO_OVERLAP")
                    .to_string(),
                config: json_object(&v.raw("config"), "Configuración (CONFIG)")?,
                auto_retry: pick_int("auto_retry", "Reintentos automáticos del grafo", 0, 30)?,
                suspend_after: pick_int(
                    "suspend_after",
                    "Suspender tras fallos seguidos",
                    0,
                    i64::from(i32::MAX),
                )?,
                // A triggered task's (one without a schedule): the form hides
                // it otherwise and sends back what it had. Then a new task
                // leaves it out and an edit keeps the server's.
                min_trigger: if v.text("schedule") == "none" {
                    pick_int(
                        "min_trigger",
                        "Intervalo mínimo entre disparos (segundos)",
                        10,
                        604_800,
                    )?
                } else {
                    g.and_then(|g| {
                        param_int(
                            &g.root_params,
                            "USER_TASK_MINIMUM_TRIGGER_INTERVAL_IN_SECONDS",
                        )
                    })
                },
                error_integration: integration(
                    "error_integration",
                    "Integración para los errores",
                )?,
                success_integration: integration(
                    "success_integration",
                    "Integración para los éxitos",
                )?,
            }
        }
    };
    let mut props = props;
    if in_object.is_some() {
        // Its state is the object's.
        props.enabled = g.is_some_and(|g| g.root.started);
    }
    let members = members_from(values, choices, &known, &name, g)?;
    if in_object.is_some() {
        let had = g.and_then(|g| {
            g.members
                .iter()
                .find(|(t, _)| t.finalizes.is_some())
                .map(|(t, _)| t.name.clone())
        });
        let has = members
            .iter()
            .find(|m| m.role == Role::Finalizer)
            .map(|m| m.original.clone().unwrap_or_default());
        if had != has {
            return Err(bad(
                "El finalizador de un grafo de tareas (objeto) se cambia en el grafo.".into(),
            ));
        }
    }
    let commit = in_object.is_some() && v.bool("commit");
    Ok(Spec {
        database,
        schema,
        name,
        props,
        root,
        members,
        commit,
    })
}

fn members_from(
    values: &FormValues,
    choices: &Choices,
    known: &HashMap<String, String>,
    root: &str,
    g: Option<&Graph>,
) -> Result<Vec<Member>> {
    let rows = rows(values, "tasks");
    if rows.len() >= MAX_GRAPH {
        return Err(bad(format!(
            "Un grafo de Snowflake tiene como mucho {MAX_GRAPH} tareas, la raíz incluida."
        )));
    }
    let current: Vec<&str> = g
        .map(|g| g.members.iter().map(|(t, _)| t.name.as_str()).collect())
        .unwrap_or_default();
    // Names first: the predecessors refer to them.
    let mut names: Vec<String> = Vec::new();
    let mut originals: Vec<Option<String>> = Vec::new();
    for row in &rows {
        let name = ident(&row.text("name"), "Nombre de la tarea")?;
        if name == root || names.contains(&name) {
            return Err(bad(format!(
                "Hay dos tareas llamadas «{name}» en el grafo."
            )));
        }
        let original = match row.opt("task") {
            Some(o) => {
                if !current.contains(&o.as_str()) {
                    return Err(bad(format!(
                        "La tarea «{}» ya no está en el grafo: volvé a abrir el formulario.",
                        shown(&o)
                    )));
                }
                if o != name {
                    return Err(bad(format!("{RENAME} («{}» → «{name}»)", shown(&o))));
                }
                Some(o)
            }
            // A new row with the name of one of the graph's tasks is that task.
            None if current.contains(&name.as_str()) => Some(name.clone()),
            None => {
                if g.is_some_and(|g| g.schema_tasks.contains(&name)) {
                    return Err(bad(format!(
                        "Ya existe una tarea «{name}» en el esquema, fuera de este grafo."
                    )));
                }
                None
            }
        };
        if let Some(o) = &original {
            if originals.iter().flatten().any(|x| x == o) {
                return Err(bad(format!("La tarea «{o}» está dos veces en el grafo.")));
            }
        }
        names.push(name);
        originals.push(original);
    }
    let mut out = Vec::new();
    let mut finalizers = 0;
    for ((row, name), original) in rows.iter().zip(&names).zip(originals) {
        let label = format!("La tarea {name}");
        let role = match row.text("role").as_str() {
            "finalizer" => {
                finalizers += 1;
                Role::Finalizer
            }
            "" | "child" => {
                let mut after: Vec<String> = Vec::new();
                if row.bool("after_root") {
                    after.push(root.to_string());
                }
                for a in row.strings("after") {
                    let p = ident(&a, "Después de")?;
                    if p == *name {
                        return Err(bad(format!("{label} no puede correr después de sí misma.")));
                    }
                    if p != root && !names.contains(&p) {
                        return Err(bad(format!(
                            "{label} corre después de «{}», que no es del grafo.",
                            shown(&a)
                        )));
                    }
                    if !after.contains(&p) {
                        after.push(p);
                    }
                }
                if after.is_empty() {
                    return Err(bad(format!(
                        "{label} necesita al menos una tarea antes («Después de la raíz» o «Después de»)."
                    )));
                }
                if after.len() > MAX_AFTER {
                    return Err(bad(format!(
                        "{label} corre después de {} tareas: Snowflake admite hasta {MAX_AFTER}.",
                        after.len()
                    )));
                }
                Role::Child(after)
            }
            other => {
                return Err(bad(format!(
                    "«{}» no es un tipo de tarea del grafo.",
                    shown(other)
                )))
            }
        };
        let params = super::params_text(&row.raw("session"), choices, known)?;
        let props = props_from(*row, choices, params, &label)?;
        out.push(Member {
            original,
            name: name.clone(),
            role,
            props,
        });
    }
    if finalizers > 1 {
        return Err(bad("Un grafo tiene un solo finalizador.".into()));
    }
    if let Some(f) = out.iter().find(|m| m.role == Role::Finalizer) {
        for m in &out {
            if matches!(&m.role, Role::Child(a) if a.contains(&f.name)) {
                return Err(bad(format!(
                    "La tarea {} no puede correr después del finalizador {}: un finalizador no tiene tareas hijas.",
                    m.name, f.name
                )));
            }
        }
    }
    order(root, &out)?;
    Ok(out)
}

/// The members in an order where each one comes after its predecessors
/// (the finalizer last); refused when they make a cycle.
pub(crate) fn order(root: &str, members: &[Member]) -> Result<Vec<usize>> {
    let mut done: BTreeSet<&str> = BTreeSet::from([root]);
    let mut out = Vec::new();
    let mut left: Vec<usize> = (0..members.len()).collect();
    while !left.is_empty() {
        let before = left.len();
        left.retain(|&i| {
            let ready = match &members[i].role {
                Role::Child(after) => after.iter().all(|a| done.contains(a.as_str())),
                Role::Finalizer => true,
            };
            if ready && matches!(members[i].role, Role::Child(_)) {
                done.insert(&members[i].name);
                out.push(i);
                false
            } else {
                true
            }
        });
        if left.len() == before {
            // Only finalizers (or a cycle) left.
            if left
                .iter()
                .all(|&i| matches!(members[i].role, Role::Finalizer))
            {
                out.append(&mut left);
                break;
            }
            let stuck: Vec<&str> = left
                .iter()
                .filter(|&&i| matches!(members[i].role, Role::Child(_)))
                .map(|&i| members[i].name.as_str())
                .collect();
            return Err(bad(format!(
                "Las tareas {} forman un ciclo: un grafo de Snowflake va siempre hacia adelante.",
                stuck.join(", ")
            )));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// What a change sets: properties (one per line) and parameters (one
/// comma-separated group, as Snowflake's grammar writes session
/// parameters), and what it unsets.
#[derive(Debug, Default)]
struct Changes {
    sets: Vec<String>,
    params: Vec<String>,
    unsets: Vec<String>,
}

impl Changes {
    /// The `SET` clauses: the properties, then the parameters' group.
    fn clauses(mut self) -> (Vec<String>, Vec<String>) {
        self.sets.sort_by_key(|s| clause_rank(s));
        if !self.params.is_empty() {
            let at = self
                .sets
                .iter()
                .position(|s| clause_rank(s) > PARAMS_RANK)
                .unwrap_or(self.sets.len());
            self.sets.insert(at, self.params.join(", "));
        }
        (self.sets, self.unsets)
    }
}

/// `ALTER TASK … SET` clauses for what differs, `UNSET` items for what
/// went away; `WHEN`, `AS` and `EXECUTE AS USER` apart.
fn prop_changes(cur: Option<&Props>, want: &Props, ch: &mut Changes) {
    let Changes {
        sets,
        params: psets,
        unsets,
    } = ch;
    let empty = Props::default();
    let c = cur.unwrap_or(&empty);
    let mut field =
        |key: &str, old: &Option<String>, new: &Option<String>, render: &dyn Fn(&str) -> String| {
            match (old, new) {
                (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
                    sets.push(format!("{key} = {}", render(n)))
                }
                (Some(_), None) => unsets.push(key.to_string()),
                _ => {}
            }
        };
    // Changing the compute: the old one goes first (UNSET runs before SET).
    field("WAREHOUSE", &c.warehouse, &want.warehouse, &|n| quote(n));
    field(
        "USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE",
        &c.initial_size,
        &want.initial_size,
        &|n| literal(n),
    );
    field(
        "SERVERLESS_TASK_MIN_STATEMENT_SIZE",
        &c.min_size,
        &want.min_size,
        &|n| literal(n),
    );
    field(
        "SERVERLESS_TASK_MAX_STATEMENT_SIZE",
        &c.max_size,
        &want.max_size,
        &|n| literal(n),
    );
    field(
        "TARGET_COMPLETION_INTERVAL",
        &c.target_completion,
        &want.target_completion,
        &|n| literal(n),
    );
    field(
        "USER_TASK_TIMEOUT_MS",
        &c.timeout_ms.map(|n| n.to_string()),
        &want.timeout_ms.map(|n| n.to_string()),
        &|n| n.to_string(),
    );
    field("LOG_LEVEL", &c.log_level, &want.log_level, &|n| literal(n));
    field("COMMENT", &c.comment, &want.comment, &|n| literal(n));
    match (&c.trace_level, &want.trace_level) {
        (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
            psets.push(format!("TRACE_LEVEL = {}", literal(n)))
        }
        (Some(_), None) => unsets.push("TRACE_LEVEL".into()),
        _ => {}
    }
    for (k, v) in &want.params {
        if !c.params.iter().any(|(ck, cv)| ck == k && cv == v) {
            psets.push(format!("{k} = {v}"));
        }
    }
    for (k, _) in &c.params {
        if !want.params.iter().any(|(wk, _)| wk == k) {
            unsets.push(k.clone());
        }
    }
}

fn root_changes(cur: Option<&RootProps>, want: &RootProps, ch: &mut Changes) {
    let Changes { sets, unsets, .. } = ch;
    let empty = RootProps {
        overlap: "NO_OVERLAP".into(),
        ..RootProps::default()
    };
    let c = cur.unwrap_or(&empty);
    match (&c.schedule, &want.schedule) {
        (o, Some(n)) if o.as_ref() != Some(n) => {
            sets.push(format!("SCHEDULE = {}", literal(&n.text(false))))
        }
        (Some(_), None) => unsets.push("SCHEDULE".into()),
        _ => {}
    }
    match (&c.config, &want.config) {
        (o, Some(n)) if !same_json(o.as_deref(), Some(n)) => {
            sets.push(format!("CONFIG = {}", literal(n)))
        }
        (Some(_), None) => unsets.push("CONFIG".into()),
        _ => {}
    }
    if c.overlap != want.overlap {
        sets.push(format!("OVERLAP_POLICY = {}", want.overlap));
    }
    let mut int = |key: &str, o: Option<i64>, n: Option<i64>| match (o, n) {
        (o, Some(n)) if o != Some(n) => sets.push(format!("{key} = {n}")),
        (Some(_), None) => unsets.push(key.to_string()),
        _ => {}
    };
    int(
        "SUSPEND_TASK_AFTER_NUM_FAILURES",
        c.suspend_after,
        want.suspend_after,
    );
    int("TASK_AUTO_RETRY_ATTEMPTS", c.auto_retry, want.auto_retry);
    int(
        "USER_TASK_MINIMUM_TRIGGER_INTERVAL_IN_SECONDS",
        c.min_trigger,
        want.min_trigger,
    );
    for (key, o, n) in [
        (
            "ERROR_INTEGRATION",
            &c.error_integration,
            &want.error_integration,
        ),
        (
            "SUCCESS_INTEGRATION",
            &c.success_integration,
            &want.success_integration,
        ),
    ] {
        match (o, n) {
            (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
                sets.push(format!("{key} = {}", quote(n)))
            }
            (Some(_), None) => unsets.push(key.to_string()),
            _ => {}
        }
    }
}

/// The `ALTER TASK` statements that take `cur` to `want` (not its state nor
/// its predecessors).
fn alter_task(
    id: &str,
    cur: &Props,
    want: &Props,
    root: Option<(&RootProps, &RootProps)>,
    out: &mut Vec<String>,
) {
    let mut ch = Changes::default();
    if let Some((c, w)) = root {
        root_changes(Some(c), w, &mut ch);
    }
    prop_changes(Some(cur), want, &mut ch);
    let (sets, unsets) = ch.clauses();
    if !unsets.is_empty() {
        out.push(format!("ALTER TASK {id} UNSET {}", unsets.join(", ")));
    }
    if !sets.is_empty() {
        out.push(format!("ALTER TASK {id} SET\n  {}", sets.join("\n  ")));
    }
    match (&cur.execute_as, &want.execute_as) {
        (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
            out.push(format!("ALTER TASK {id} SET EXECUTE AS USER {}", quote(n)))
        }
        (Some(_), None) => out.push(format!("ALTER TASK {id} UNSET EXECUTE AS USER")),
        _ => {}
    }
    match (&cur.condition, &want.condition) {
        (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
            out.push(format!("ALTER TASK {id} MODIFY WHEN {n}"))
        }
        (Some(_), None) => out.push(format!("ALTER TASK {id} REMOVE WHEN")),
        _ => {}
    }
    if cur.definition != want.definition {
        out.push(format!("ALTER TASK {id} MODIFY AS\n{}", want.definition));
    }
}

/// `CREATE TASK` for a task of the spec: the root (`root`), or a member
/// (`role`).
fn create_task(
    s: &Spec,
    name: &str,
    p: &Props,
    root: Option<&RootProps>,
    role: Option<&Role>,
) -> String {
    let mut lines = vec![format!("CREATE TASK {}", s.task_id(name))];
    let mut ch = Changes::default();
    if let Some(r) = root {
        root_changes(None, r, &mut ch);
    }
    prop_changes(None, p, &mut ch);
    // Snowflake's order: properties, then AFTER, EXECUTE AS USER, WHEN, AS.
    let (sets, _) = ch.clauses();
    lines.extend(sets.into_iter().map(|s| format!("  {s}")));
    match role {
        Some(Role::Finalizer) => lines.push(format!("  FINALIZE = {}", s.id())),
        Some(Role::Child(after)) => {
            let ids: Vec<String> = after.iter().map(|a| s.task_id(a)).collect();
            lines.push(format!("  AFTER {}", ids.join(", ")));
        }
        None => {}
    }
    if let Some(u) = &p.execute_as {
        lines.push(format!("  EXECUTE AS USER {}", quote(u)));
    }
    if let Some(c) = &p.condition {
        lines.push(format!("  WHEN {c}"));
    }
    lines.push("AS".into());
    lines.push(p.definition.clone());
    lines.join("\n")
}

/// Where the session parameters' group goes among the properties.
const PARAMS_RANK: usize = 5;

/// The documented order of `CREATE TASK`'s properties (session parameters
/// after OVERLAP_POLICY).
fn clause_rank(clause: &str) -> usize {
    const ORDER: [&str; 16] = [
        "WAREHOUSE",
        "USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE",
        "SCHEDULE",
        "CONFIG",
        "OVERLAP_POLICY",
        "USER_TASK_TIMEOUT_MS",
        "SUSPEND_TASK_AFTER_NUM_FAILURES",
        "ERROR_INTEGRATION",
        "SUCCESS_INTEGRATION",
        "LOG_LEVEL",
        "COMMENT",
        "TASK_AUTO_RETRY_ATTEMPTS",
        "USER_TASK_MINIMUM_TRIGGER_INTERVAL_IN_SECONDS",
        "TARGET_COMPLETION_INTERVAL",
        "SERVERLESS_TASK_MIN_STATEMENT_SIZE",
        "SERVERLESS_TASK_MAX_STATEMENT_SIZE",
    ];
    let key = clause.split(" = ").next().unwrap_or_default();
    match ORDER.iter().position(|k| *k == key) {
        Some(i) if i < PARAMS_RANK => i,
        Some(i) => i + 1,
        None => PARAMS_RANK,
    }
}

fn header(s: &Spec, new: bool) -> String {
    format!(
        "Tarea {}.{}.{} de Snowflake{}",
        s.database,
        s.schema,
        s.name,
        if new { " (nueva)" } else { "" }
    )
}

/// The script that creates the graph: the root, its tasks in order, then
/// the resumes (children before the root).
pub(crate) fn create_plan(s: &Spec) -> Result<Plan> {
    let mut p = Plan::new(Some(s.id()), &header(s, true));
    p.push(create_task(s, &s.name, &s.props, Some(&s.root), None));
    for i in order(&s.name, &s.members)? {
        let m = &s.members[i];
        p.push(create_task(s, &m.name, &m.props, None, Some(&m.role)));
    }
    let resumes: Vec<String> = s
        .members
        .iter()
        .filter(|m| m.props.enabled)
        .map(|m| format!("ALTER TASK {} RESUME", s.task_id(&m.name)))
        .collect();
    if !resumes.is_empty() || s.props.enabled {
        p.note(NEW_SUSPENDED);
    }
    for r in resumes {
        p.push(r);
    }
    if s.props.enabled {
        p.push(format!("ALTER TASK {} RESUME", s.id()));
    }
    Ok(p)
}

/// The script that takes the server's graph (`cur`, `started`: its root
/// runs) to the form's (`want`), in place.
pub(crate) fn alter_plan(
    cur: &Spec,
    want: &Spec,
    started: bool,
    object: Option<&str>,
) -> Result<Plan> {
    let id = want.id();
    let mut mid: Vec<String> = Vec::new();
    let root_cur = (object.is_none()).then_some(&cur.root);
    alter_task(
        &id,
        &cur.props,
        &want.props,
        root_cur.map(|c| (c, &want.root)),
        &mut mid,
    );
    let by_name: HashMap<&str, &Member> =
        cur.members.iter().map(|m| (m.name.as_str(), m)).collect();
    let kept: Vec<&Member> = want
        .members
        .iter()
        .filter(|m| m.original.is_some())
        .collect();
    let kept_names: BTreeSet<&str> = kept.iter().map(|m| m.name.as_str()).collect();
    let dropped: Vec<&Member> = cur
        .members
        .iter()
        .filter(|m| !kept_names.contains(m.name.as_str()))
        .collect();
    let dropped_names: BTreeSet<&str> = dropped.iter().map(|m| m.name.as_str()).collect();
    let new_names: BTreeSet<&str> = want
        .members
        .iter()
        .filter(|m| m.original.is_none())
        .map(|m| m.name.as_str())
        .collect();
    let preds = |r: &Role| -> Vec<String> {
        match r {
            Role::Child(a) => a.clone(),
            Role::Finalizer => Vec::new(),
        }
    };
    // 1. Finalizers that become children stop being finalizers first.
    for m in &kept {
        let c = by_name[m.name.as_str()];
        if c.role == Role::Finalizer && m.role != Role::Finalizer {
            mid.push(format!(
                "ALTER TASK {} UNSET FINALIZE",
                want.task_id(&m.name)
            ));
        }
    }
    // 2. Each kept task's properties, and the predecessors it gains that
    //    exist already (before any drop, so nothing loses its last one).
    for m in &kept {
        let c = by_name[m.name.as_str()];
        let tid = want.task_id(&m.name);
        alter_task(&tid, &c.props, &m.props, None, &mut mid);
        let have = preds(&c.role);
        let add: Vec<String> = preds(&m.role)
            .into_iter()
            .filter(|a| !have.contains(a) && !new_names.contains(a.as_str()))
            .map(|a| want.task_id(&a))
            .collect();
        if !add.is_empty() {
            mid.push(format!("ALTER TASK {tid} ADD AFTER {}", add.join(", ")));
        }
    }
    // 3. The tasks that leave the graph.
    for m in &dropped {
        mid.push(format!("DROP TASK {}", want.task_id(&m.name)));
    }
    // 4. The new ones, each after its predecessors.
    for i in order(&want.name, &want.members)? {
        let m = &want.members[i];
        if m.original.is_none() {
            mid.push(create_task(want, &m.name, &m.props, None, Some(&m.role)));
        }
    }
    // 5. Predecessors that are new tasks, then the ones a task no longer
    //    runs after; then the tasks that become the finalizer.
    for m in &kept {
        let c = by_name[m.name.as_str()];
        let tid = want.task_id(&m.name);
        let have = preds(&c.role);
        let add: Vec<String> = preds(&m.role)
            .into_iter()
            .filter(|a| !have.contains(a) && new_names.contains(a.as_str()))
            .map(|a| want.task_id(&a))
            .collect();
        if !add.is_empty() {
            mid.push(format!("ALTER TASK {tid} ADD AFTER {}", add.join(", ")));
        }
    }
    for m in &kept {
        let c = by_name[m.name.as_str()];
        let tid = want.task_id(&m.name);
        let wanted = preds(&m.role);
        let remove: Vec<String> = preds(&c.role)
            .into_iter()
            .filter(|a| !wanted.contains(a) && !dropped_names.contains(a.as_str()))
            .map(|a| want.task_id(&a))
            .collect();
        if !remove.is_empty() {
            mid.push(format!(
                "ALTER TASK {tid} REMOVE AFTER {}",
                remove.join(", ")
            ));
        }
    }
    for m in &kept {
        let c = by_name[m.name.as_str()];
        if m.role == Role::Finalizer && c.role != Role::Finalizer {
            mid.push(format!(
                "ALTER TASK {} SET FINALIZE = {}",
                want.task_id(&m.name),
                id
            ));
        }
    }
    // 6. The members' states: new tasks start suspended, and one whose
    //    predecessors changed may have been suspended on the way.
    for m in &want.members {
        let tid = want.task_id(&m.name);
        match m.original.as_deref().map(|o| by_name[o]) {
            None if m.props.enabled => mid.push(format!("ALTER TASK {tid} RESUME")),
            None => {}
            Some(c) => {
                let moved = !same_role(&c.role, &m.role)
                    || preds(&c.role)
                        .iter()
                        .any(|a| dropped_names.contains(a.as_str()));
                if m.props.enabled && (!c.props.enabled || moved) {
                    mid.push(format!("ALTER TASK {tid} RESUME"));
                } else if !m.props.enabled && c.props.enabled {
                    mid.push(format!("ALTER TASK {tid} SUSPEND"));
                }
            }
        }
    }
    let mut p = Plan::new(Some(id.clone()), &header(want, false));
    if !dropped.is_empty() {
        let names: Vec<&str> = dropped.iter().map(|m| m.name.as_str()).collect();
        p.warn(&format!(
            "Las tareas que se quitaron del grafo se borran: {}.",
            names.join(", ")
        ));
    }
    match object {
        Some(g) => {
            let any = !mid.is_empty();
            p.steps = mid;
            if any && want.commit {
                p.note(STAGED);
                p.push(format!("ALTER TASK GRAPH {g} COMMIT"));
            }
        }
        None => {
            let changes = !mid.is_empty();
            if changes && started {
                p.note(SUSPENDS);
                p.push(format!("ALTER TASK {id} SUSPEND"));
                p.restore = Some(format!("ALTER TASK {id} RESUME"));
            }
            p.steps.extend(mid);
            if want.props.enabled && (!started || changes) {
                p.push(format!("ALTER TASK {id} RESUME"));
            } else if !want.props.enabled && started && !changes {
                p.push(format!("ALTER TASK {id} SUSPEND"));
            }
            if changes && started && !want.props.enabled {
                // Suspended for the change and left so.
                p.restore = None;
            }
        }
    }
    Ok(p)
}

/// The same role, predecessors in any order.
fn same_role(a: &Role, b: &Role) -> bool {
    match (a, b) {
        (Role::Child(x), Role::Child(y)) => {
            x.iter().collect::<BTreeSet<_>>() == y.iter().collect::<BTreeSet<_>>()
        }
        (Role::Finalizer, Role::Finalizer) => true,
        _ => false,
    }
}

pub(crate) async fn plan(r: Run<'_>, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let choices = load(
        r,
        &["warehouse", "notification_integration", "session_parameter"],
    )
    .await;
    match id {
        None => {
            let mut s = spec(values, &choices, None)?;
            // The schema exists and the names are free.
            let taken: BTreeSet<String> = schema_tasks(r, &s.database, &s.schema)
                .await?
                .into_iter()
                .map(|t| t.name)
                .collect();
            for n in std::iter::once(&s.name).chain(s.members.iter().map(|m| &m.name)) {
                if taken.contains(n) {
                    return Err(bad(format!(
                        "Ya existe una tarea «{n}» en {}.{}.",
                        s.database, s.schema
                    )));
                }
            }
            s.commit = false;
            create_plan(&s)
        }
        Some(id) => {
            let g = read(r, id).await?;
            may_change(r, &g).await?;
            let stale = Vals(&values.values).opt("sf_id");
            if let (Some(seen), Some(now)) = (stale, &g.root.sf_id) {
                if &seen != now {
                    return Err(bad("La tarea se volvió a crear desde que se abrió el formulario: volvé a abrirlo.".into()));
                }
            }
            let cur = current(&g);
            // What the graph has now is taken even when the role can't
            // list it (a warehouse it has no privilege on).
            let mut choices = choices;
            for p in std::iter::once(&cur.props).chain(cur.members.iter().map(|m| &m.props)) {
                if let Some(w) = &p.warehouse {
                    choices.keep("warehouse", w, w);
                }
                for (k, _) in &p.params {
                    choices.keep("session_parameter", k, k);
                }
            }
            for n in [&cur.root.error_integration, &cur.root.success_integration]
                .into_iter()
                .flatten()
            {
                choices.keep("notification_integration", n, n);
            }
            let want = spec(values, &choices, Some(&g))?;
            let mut p = alter_plan(&cur, &want, g.root.started, g.root.graph.as_deref())?;
            // The task graph object's COMMIT publishes whatever it had
            // staged before too: a warning when Snowflake says it has some,
            // a note that it may when Snowflake doesn't say.
            if let Some(obj) = g.root.graph.as_deref() {
                let commits = p
                    .steps
                    .iter()
                    .any(|s| s.starts_with("ALTER TASK GRAPH ") && s.ends_with(" COMMIT"));
                if commits {
                    match super::graph::has_pending(r, obj).await {
                        Some(true) => p.warn(super::PENDING_COMMIT),
                        Some(false) => {}
                        None => {
                            p.header.retain(|h| h != STAGED);
                            p.note(super::MAY_PUBLISH_EARLIER);
                        }
                    }
                }
            }
            Ok(p)
        }
    }
}

async fn may_change(r: Run<'_>, g: &Graph) -> Result<()> {
    if g.orphan {
        return Err(bad(ORPHAN.into()));
    }
    if g.root.app_owned() {
        return Err(bad(APP_OWNED.into()));
    }
    if let Some(owner) = &g.root.owner {
        if !held(r, std::slice::from_ref(owner))
            .await
            .get(owner)
            .copied()
            .unwrap_or(true)
        {
            return Err(bad(NOT_OWNER.into()));
        }
    }
    Ok(())
}

/// Dropping the graph: its root suspended first (Snowflake drops a graph's
/// tasks only so), the finalizer and the children, the root last.
pub(crate) fn drop_plan(g: &Graph) -> Result<Plan> {
    let id = g.root.id();
    if let Some(o) = &g.root.graph {
        return Err(bad(format!(
            "La tarea es la raíz del grafo de tareas {o}: Snowflake no borra las tareas de su versión publicada. Borrá antes el grafo (o sacá la tarea de él y publicá el cambio)."
        )));
    }
    let mut p = Plan::new(
        None,
        &format!(
            "Borra la tarea {}.{}.{} de Snowflake{}",
            g.root.database,
            g.root.schema,
            g.root.name,
            match g.members.len() {
                0 => String::new(),
                1 => " y la otra tarea de su grafo".to_string(),
                n => format!(" y las {n} tareas de su grafo"),
            }
        ),
    );
    if !g.members.is_empty() {
        p.warn(super::GRAPH_GOES);
    }
    if g.root.started && !g.members.is_empty() {
        p.push(format!("ALTER TASK {id} SUSPEND"));
    }
    for (t, _) in g.members.iter().rev() {
        p.push(format!("DROP TASK {}", t.id()));
    }
    p.push(format!("DROP TASK {id}"));
    Ok(p)
}

pub(crate) async fn delete_plan(r: Run<'_>, id: &str) -> Result<Plan> {
    let g = read(r, id).await?;
    may_change(r, &g).await?;
    drop_plan(&g)
}

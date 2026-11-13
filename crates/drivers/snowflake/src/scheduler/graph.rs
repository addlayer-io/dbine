//! The `task_graph` kind: task graph objects (a preview feature of
//! Snowflake): a graph's schedule, root, finalizer, overlap,
//! notifications, configuration, retries and the defaults of its member
//! tasks (`TASK_DEFAULTS`). `CREATE TASK GRAPH`, `ALTER TASK GRAPH … SET |
//! UNSET | SET TASK_DEFAULTS | UNSET TASK_DEFAULTS | MODIFY WHEN | REMOVE
//! WHEN | COMMIT | RESUME | SUSPEND`, `DROP TASK GRAPH` (suspended first).
//! Changes are staged until a `COMMIT`, which the form does by default.
//! The member tasks themselves are the `job` kind's (their root's graph).

use super::task::{self, Shown};
use super::{
    bad, checked_id, completion_interval, expression, held, ident, json_object, load, log_select,
    overlap_select, same_json, schedule_fields, schedule_of, schedule_values, shown, shown_ident,
    size_key, size_select, trace_select, Choices, Kind, Plan, Run, Schedule, Vals, LOG_LEVELS,
    OVERLAPS, SIZES, TRACE_LEVELS,
};
use crate::jobs::{self, get, job_id, literal, text};
use crate::monitor::{quote, Set};
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const NOT_OWNER: &str = "Solo el rol propietario del grafo (y los roles que lo heredan) puede cambiarlo o borrarlo: el rol de la sesión no lo tiene.";
const UNKNOWN_PARAMS: &str = "Snowflake no muestra los valores de TASK_AUTO_RETRY_ATTEMPTS ni de SUSPEND_TASK_AFTER_NUM_FAILURES de un grafo: vacío deja el que tiene.";

/// One row of `SHOW TASK GRAPHS`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct GraphRow {
    pub(crate) database: String,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) sf_id: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) schedule: Option<String>,
    pub(crate) started: bool,
    pub(crate) defaults: Option<String>,
    pub(crate) pending: Option<String>,
    pub(crate) overlap: Option<String>,
    pub(crate) config: Option<String>,
    pub(crate) success_integration: Option<String>,
    pub(crate) error_integration: Option<String>,
    pub(crate) condition: Option<String>,
    pub(crate) created_on: Option<String>,
    pub(crate) last_suspended_on: Option<String>,
    pub(crate) last_suspended_reason: Option<String>,
}

impl GraphRow {
    pub(crate) fn id(&self) -> String {
        job_id(&self.database, &self.schema, &self.name)
    }
}

pub(crate) fn graph_rows(s: &Set) -> Vec<GraphRow> {
    (0..s.rows.len())
        .filter_map(|i| {
            Some(GraphRow {
                database: text(s, i, "database_name")?,
                schema: text(s, i, "schema_name")?,
                name: text(s, i, "name")?,
                sf_id: text(s, i, "id"),
                owner: text(s, i, "owner"),
                comment: text(s, i, "comment"),
                schedule: text(s, i, "schedule"),
                started: get(s, i, "state").is_some_and(|v| v.eq_ignore_ascii_case("started")),
                defaults: text(s, i, "task_defaults"),
                pending: text(s, i, "has_pending_changes"),
                overlap: text(s, i, "overlap_policy"),
                config: text(s, i, "config"),
                success_integration: text(s, i, "success_integration"),
                error_integration: text(s, i, "error_integration"),
                condition: text(s, i, "condition"),
                created_on: jobs::shown_time(get(s, i, "created_on")),
                last_suspended_on: jobs::shown_time(get(s, i, "last_suspended_on")),
                last_suspended_reason: text(s, i, "last_suspended_reason"),
            })
        })
        .collect()
}

pub(crate) async fn items(r: Run<'_>) -> Result<SchedulerList> {
    match r.rows("SHOW TASK GRAPHS IN ACCOUNT").await {
        Ok(s) => {
            let mut items: Vec<SchedulerItem> = graph_rows(&s)
                .iter()
                .map(|g| SchedulerItem {
                    id: g.id(),
                    name: g.name.clone(),
                    description: g.comment.clone(),
                    enabled: Some(g.started),
                    system: false,
                    can_edit: None,
                    can_delete: None,
                    actions: None,
                })
                .collect();
            items.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(SchedulerList {
                items,
                notes: Vec::new(),
            })
        }
        // A preview the account may not have, or a role that can't list
        // them: an empty list that says why.
        Err(e) => Ok(SchedulerList {
            items: Vec::new(),
            notes: vec![format!(
                "Esta cuenta no lista grafos de tareas (TASK GRAPH, en vista previa): {e}"
            )],
        }),
    }
}

/// The graph, its root's and finalizer's names, and its schema's tasks
/// (what the root and the finalizer are picked from).
pub(crate) struct Read {
    pub(crate) graph: GraphRow,
    pub(crate) root: Option<String>,
    pub(crate) finalizer: Option<String>,
    pub(crate) tasks: Vec<Shown>,
}

async fn read(r: Run<'_>, id: &str) -> Result<Read> {
    let (db, schema, name) = checked_id(Kind::Graph, id)?;
    let graphs_sql = format!(
        "SHOW TASK GRAPHS LIKE {} IN SCHEMA {}.{}",
        literal(&name),
        quote(&db),
        quote(&schema)
    );
    let tasks_sql = format!("SHOW TASKS IN SCHEMA {}.{}", quote(&db), quote(&schema));
    let (graphs, tasks) = tokio::join!(r.rows(&graphs_sql), r.rows(&tasks_sql));
    let graph = graph_rows(&graphs?)
        .into_iter()
        .find(|g| g.name == name && g.database == db && g.schema == schema)
        .ok_or_else(|| {
            bad(format!(
                "No existe el grafo de tareas {db}.{schema}.{name} o el rol no lo ve."
            ))
        })?;
    let tasks = task::shown_rows(&tasks?);
    let gid = graph.id();
    let root = tasks
        .iter()
        .find(|t| t.graph.as_deref() == Some(gid.as_str()))
        .map(|t| t.name.clone());
    let finalizer = tasks
        .iter()
        .find(|t| t.finalizes_graph.as_deref() == Some(gid.as_str()))
        .map(|t| t.name.clone());
    Ok(Read {
        graph,
        root,
        finalizer,
        tasks,
    })
}

// ---------------------------------------------------------------------------
// What a graph is: the same shape for the server's and the form's
// ---------------------------------------------------------------------------

/// `TASK_DEFAULTS`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Defaults {
    pub(crate) warehouse: Option<String>,
    pub(crate) initial_size: Option<String>,
    pub(crate) min_size: Option<String>,
    pub(crate) max_size: Option<String>,
    pub(crate) target_completion: Option<String>,
    pub(crate) timeout_ms: Option<i64>,
    pub(crate) execute_as: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) log_level: Option<String>,
    pub(crate) trace_level: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Spec {
    pub(crate) database: String,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) schedule: Option<Schedule>,
    pub(crate) root: Option<String>,
    pub(crate) finalizer: Option<String>,
    pub(crate) overlap: String,
    pub(crate) error_integration: Option<String>,
    pub(crate) success_integration: Option<String>,
    pub(crate) config: Option<String>,
    /// `None`: keep (Snowflake doesn't show them).
    pub(crate) auto_retry: Option<i64>,
    pub(crate) suspend_after: Option<i64>,
    pub(crate) comment: Option<String>,
    pub(crate) defaults: Defaults,
    pub(crate) condition: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) commit: bool,
}

impl Spec {
    fn id(&self) -> String {
        job_id(&self.database, &self.schema, &self.name)
    }

    fn task_id(&self, name: &str) -> String {
        job_id(&self.database, &self.schema, name)
    }
}

/// `task_defaults`' JSON (keys compared without case, spaces or `_`).
pub(crate) fn defaults_of(v: Option<&str>) -> Defaults {
    let Some(Value::Object(m)) = v.and_then(|v| serde_json::from_str::<Value>(v).ok()) else {
        return Defaults::default();
    };
    let get = |key: &str| -> Option<String> {
        let want: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        m.iter()
            .find(|(k, _)| {
                k.chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .eq_ignore_ascii_case(&want)
            })
            .and_then(|(_, v)| match v {
                Value::String(s) => Some(s.trim().to_string()),
                Value::Null => None,
                v => Some(v.to_string()),
            })
            .filter(|s| !s.is_empty())
    };
    Defaults {
        warehouse: get("WAREHOUSE"),
        initial_size: get("USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE").map(|s| size_key(&s)),
        min_size: get("SERVERLESS_TASK_MIN_STATEMENT_SIZE").map(|s| size_key(&s)),
        max_size: get("SERVERLESS_TASK_MAX_STATEMENT_SIZE").map(|s| size_key(&s)),
        target_completion: get("TARGET_COMPLETION_INTERVAL")
            .map(|t| completion_interval(&t, "").ok().flatten().unwrap_or(t)),
        timeout_ms: get("USER_TASK_TIMEOUT_MS")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|f| f as i64),
        execute_as: get("EXECUTE_AS_USER"),
        comment: get("COMMENT"),
        log_level: get("LOG_LEVEL").map(|s| s.to_ascii_uppercase()),
        trace_level: get("TRACE_LEVEL").map(|s| s.to_ascii_uppercase()),
    }
}

pub(crate) fn current(rd: &Read) -> Spec {
    let g = &rd.graph;
    Spec {
        database: g.database.clone(),
        schema: g.schema.clone(),
        name: g.name.clone(),
        schedule: g.schedule.as_deref().and_then(Schedule::parse),
        root: rd.root.clone(),
        finalizer: rd.finalizer.clone(),
        overlap: g.overlap.clone().unwrap_or_else(|| "NO_OVERLAP".into()),
        error_integration: g.error_integration.clone(),
        success_integration: g.success_integration.clone(),
        config: g.config.clone(),
        auto_retry: None,
        suspend_after: None,
        comment: g.comment.clone(),
        defaults: defaults_of(g.defaults.as_deref()),
        condition: g
            .condition
            .as_deref()
            .map(super::trim_code)
            .filter(|c| !c.is_empty()),
        enabled: g.started,
        commit: false,
    }
}

/// A task picked for the root or the finalizer: one of the graph's schema.
fn member(v: Vals, k: &str, label: &str, db: &str, schema: &str) -> Result<Option<String>> {
    let Some(t) = v.opt(k) else {
        return Ok(None);
    };
    // The list's values are ids; a name typed alone is the schema's.
    let name = match jobs::parse_id(&t) {
        Some((d, s, n)) => {
            if d != db || s != schema {
                return Err(bad(format!(
                    "«{label}»: la tarea tiene que estar en el esquema del grafo ({db}.{schema})."
                )));
            }
            n
        }
        None => ident(&t, label)?,
    };
    Ok(Some(name))
}

pub(crate) fn spec(values: &FormValues, choices: &Choices, cur: Option<&Read>) -> Result<Spec> {
    let v = Vals(&values.values);
    let (database, schema, name) = match cur {
        Some(rd) => {
            if !v.text("name").is_empty() && ident(&v.text("name"), "Nombre")? != rd.graph.name {
                return Err(bad(
                    "Snowflake no permite renombrar un grafo de tareas.".into()
                ));
            }
            (
                rd.graph.database.clone(),
                rd.graph.schema.clone(),
                rd.graph.name.clone(),
            )
        }
        None => {
            let sch = v
                .opt("schema")
                .ok_or_else(|| bad("Elegí el esquema del grafo.".into()))?;
            let (db, s) = super::parse_schema(&sch)
                .ok_or_else(|| bad(format!("«{}» no es un esquema de Snowflake.", shown(&sch))))?;
            (db, s, ident(&v.text("name"), "Nombre")?)
        }
    };
    let root = member(v, "root", "Tarea raíz", &database, &schema)?;
    let finalizer = member(v, "finalizer", "Finalizador", &database, &schema)?;
    if root.is_some() && root == finalizer {
        return Err(bad(
            "La raíz y el finalizador no pueden ser la misma tarea.".into(),
        ));
    }
    if let Some(rd) = cur {
        for (t, l) in [(&root, "Tarea raíz"), (&finalizer, "Finalizador")] {
            if let Some(t) = t {
                if !rd.tasks.iter().any(|x| &x.name == t) {
                    return Err(bad(format!(
                        "«{l}»: no existe la tarea {t} en {database}.{schema}."
                    )));
                }
            }
        }
    }
    let integration = |k: &str, l: &str| -> Result<Option<String>> {
        match v.opt(k) {
            Some(n) => Ok(Some(choices.pick("notification_integration", &n, l)?)),
            None => Ok(None),
        }
    };
    let size = |k: &str, l: &str| -> Result<Option<String>> {
        Ok(v.pick(k, l, &SIZES, None)?.map(str::to_string))
    };
    let defaults = Defaults {
        warehouse: match v.opt("d_warehouse") {
            Some(w) => Some(choices.pick("warehouse", &w, "Warehouse")?),
            None => None,
        },
        initial_size: size("d_initial_size", "Tamaño inicial")?,
        min_size: size("d_min_size", "Tamaño mínimo")?,
        max_size: size("d_max_size", "Tamaño máximo")?,
        target_completion: completion_interval(
            &v.text("d_target_completion"),
            "Tiempo objetivo de finalización",
        )?,
        timeout_ms: v.int(
            "d_timeout_ms",
            "Tiempo máximo de ejecución (ms)",
            0,
            604_800_000,
        )?,
        execute_as: match v.opt("d_execute_as") {
            Some(u) => Some(ident(&u, "Ejecutar como usuario")?),
            None => None,
        },
        comment: v.opt("d_comment"),
        log_level: v
            .pick("d_log_level", "Nivel de registro", &LOG_LEVELS, None)?
            .map(str::to_string),
        trace_level: v
            .pick("d_trace_level", "Nivel de trazas", &TRACE_LEVELS, None)?
            .map(str::to_string),
    };
    let enabled = v.bool("enabled");
    let schedule = schedule_of(v, false)?;
    let condition = expression(&v.raw("condition"), "Condición (WHEN)")?;
    if enabled && schedule.is_none() && condition.is_none() {
        return Err(bad(
            "Un grafo de tareas activo necesita una programación o una condición (WHEN).".into(),
        ));
    }
    Ok(Spec {
        database,
        schema,
        name,
        schedule,
        root,
        finalizer,
        overlap: v
            .pick(
                "overlap",
                "Ejecuciones superpuestas",
                &OVERLAPS,
                Some("NO_OVERLAP"),
            )?
            .unwrap_or("NO_OVERLAP")
            .to_string(),
        error_integration: integration("error_integration", "Integración para los errores")?,
        success_integration: integration("success_integration", "Integración para los éxitos")?,
        config: json_object(&v.raw("config"), "Configuración (CONFIG)")?,
        auto_retry: v.int("auto_retry", "Reintentos automáticos del grafo", 0, 30)?,
        suspend_after: v.int(
            "suspend_after",
            "Suspender tras fallos seguidos",
            0,
            i64::from(i32::MAX),
        )?,
        comment: v.opt("comment"),
        defaults,
        condition,
        enabled,
        commit: v.bool("commit"),
    })
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// `TASK_DEFAULTS` entries that differ (`k = v`) and the ones that went
/// away.
fn default_changes(c: &Defaults, w: &Defaults) -> (Vec<String>, Vec<String>) {
    let mut sets = Vec::new();
    let mut unsets = Vec::new();
    let mut f = |key: &str,
                 o: &Option<String>,
                 n: &Option<String>,
                 render: &dyn Fn(&str) -> String| {
        match (o, n) {
            (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
                sets.push(format!("{key} = {}", render(n)))
            }
            (Some(_), None) => unsets.push(key.to_string()),
            _ => {}
        }
    };
    f("WAREHOUSE", &c.warehouse, &w.warehouse, &|n| quote(n));
    f(
        "USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE",
        &c.initial_size,
        &w.initial_size,
        &|n| literal(n),
    );
    f(
        "TARGET_COMPLETION_INTERVAL",
        &c.target_completion,
        &w.target_completion,
        &|n| literal(n),
    );
    f(
        "SERVERLESS_TASK_MIN_STATEMENT_SIZE",
        &c.min_size,
        &w.min_size,
        &|n| literal(n),
    );
    f(
        "SERVERLESS_TASK_MAX_STATEMENT_SIZE",
        &c.max_size,
        &w.max_size,
        &|n| literal(n),
    );
    f(
        "USER_TASK_TIMEOUT_MS",
        &c.timeout_ms.map(|n| n.to_string()),
        &w.timeout_ms.map(|n| n.to_string()),
        &|n| n.to_string(),
    );
    f("COMMENT", &c.comment, &w.comment, &|n| literal(n));
    f("LOG_LEVEL", &c.log_level, &w.log_level, &|n| literal(n));
    f("TRACE_LEVEL", &c.trace_level, &w.trace_level, &|n| {
        literal(n)
    });
    match (&c.execute_as, &w.execute_as) {
        (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
            sets.push(format!("EXECUTE AS USER {}", quote(n)))
        }
        (Some(_), None) => unsets.push("EXECUTE AS USER".into()),
        _ => {}
    }
    (sets, unsets)
}

/// The graph's own properties that differ, and the ones that went away.
fn graph_changes(c: Option<&Spec>, w: &Spec) -> (Vec<String>, Vec<String>) {
    let mut sets = Vec::new();
    let mut unsets = Vec::new();
    match (c.and_then(|c| c.schedule.clone()), &w.schedule) {
        (o, Some(n)) if o.as_ref() != Some(n) => {
            sets.push(format!("SCHEDULE = {}", literal(&n.text(false))))
        }
        (Some(_), None) => unsets.push("SCHEDULE".into()),
        _ => {}
    }
    for (key, o, n) in [
        ("ROOTS", c.and_then(|c| c.root.clone()), &w.root),
        (
            "FINALIZER",
            c.and_then(|c| c.finalizer.clone()),
            &w.finalizer,
        ),
    ] {
        match (o, n) {
            (o, Some(n)) if o.as_deref() != Some(n.as_str()) => sets.push(if key == "ROOTS" {
                format!("ROOTS = ( {} )", w.task_id(n))
            } else {
                format!("FINALIZER = {}", w.task_id(n))
            }),
            (Some(_), None) => unsets.push(key.to_string()),
            _ => {}
        }
    }
    let c_overlap = c.map_or("NO_OVERLAP", |c| c.overlap.as_str());
    if c_overlap != w.overlap {
        sets.push(format!("OVERLAP_POLICY = {}", w.overlap));
    }
    for (key, o, n) in [
        (
            "ERROR_INTEGRATION",
            c.and_then(|c| c.error_integration.clone()),
            &w.error_integration,
        ),
        (
            "SUCCESS_INTEGRATION",
            c.and_then(|c| c.success_integration.clone()),
            &w.success_integration,
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
    match (c.and_then(|c| c.config.clone()), &w.config) {
        (o, Some(n)) if !same_json(o.as_deref(), Some(n)) => {
            sets.push(format!("CONFIG = {}", literal(n)))
        }
        (Some(_), None) => unsets.push("CONFIG".into()),
        _ => {}
    }
    // Not shown by Snowflake: a value is set, empty keeps what it has.
    if let Some(n) = w.auto_retry {
        sets.push(format!("TASK_AUTO_RETRY_ATTEMPTS = {n}"));
    }
    if let Some(n) = w.suspend_after {
        sets.push(format!("SUSPEND_TASK_AFTER_NUM_FAILURES = {n}"));
    }
    match (c.and_then(|c| c.comment.clone()), &w.comment) {
        (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
            sets.push(format!("COMMENT = {}", literal(n)))
        }
        (Some(_), None) => unsets.push("COMMENT".into()),
        _ => {}
    }
    (sets, unsets)
}

fn header(s: &Spec, new: bool) -> String {
    format!(
        "Grafo de tareas {}.{}.{} de Snowflake{}",
        s.database,
        s.schema,
        s.name,
        if new { " (nuevo)" } else { "" }
    )
}

pub(crate) fn create_plan(s: &Spec) -> Plan {
    let id = s.id();
    let mut p = Plan::new(Some(id.clone()), &header(s, true));
    let (sets, _) = graph_changes(None, s);
    let mut lines = vec![format!("CREATE TASK GRAPH {id}")];
    lines.extend(sets.into_iter().map(|l| format!("  {l}")));
    let (defs, _) = default_changes(&Defaults::default(), &s.defaults);
    if !defs.is_empty() {
        lines.push(format!("  TASK_DEFAULTS ( {} )", defs.join(", ")));
    }
    if let Some(c) = &s.condition {
        lines.push(format!("  WHEN {c}"));
    }
    p.push(lines.join("\n"));
    if (s.commit || s.enabled) && s.root.is_some() {
        p.note("Un grafo de tareas corre la versión publicada: el COMMIT la publica.");
        p.push(format!("ALTER TASK GRAPH {id} COMMIT"));
    }
    if s.enabled {
        p.push(format!("ALTER TASK GRAPH {id} RESUME"));
    }
    p
}

pub(crate) fn alter_plan(cur: &Spec, want: &Spec, pending: bool) -> Plan {
    let id = want.id();
    let mut p = Plan::new(Some(id.clone()), &header(want, false));
    let (sets, unsets) = graph_changes(Some(cur), want);
    if !unsets.is_empty() {
        p.push(format!("ALTER TASK GRAPH {id} UNSET {}", unsets.join(", ")));
    }
    if !sets.is_empty() {
        p.push(format!(
            "ALTER TASK GRAPH {id} SET\n  {}",
            sets.join(",\n  ")
        ));
    }
    let (dsets, dunsets) = default_changes(&cur.defaults, &want.defaults);
    if !dunsets.is_empty() {
        p.push(format!(
            "ALTER TASK GRAPH {id} UNSET TASK_DEFAULTS ( {} )",
            dunsets.join(", ")
        ));
    }
    if !dsets.is_empty() {
        p.push(format!(
            "ALTER TASK GRAPH {id} SET TASK_DEFAULTS ( {} )",
            dsets.join(", ")
        ));
    }
    match (&cur.condition, &want.condition) {
        (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
            p.push(format!("ALTER TASK GRAPH {id} MODIFY WHEN {n}"))
        }
        (Some(_), None) => p.push(format!("ALTER TASK GRAPH {id} REMOVE WHEN")),
        _ => {}
    }
    let changed = !p.steps.is_empty();
    if want.commit && (changed || pending) && want.root.is_some() {
        p.note("Los cambios quedan pendientes hasta el COMMIT, que también publica los que hubiera pendientes de antes.");
        p.push(format!("ALTER TASK GRAPH {id} COMMIT"));
    }
    if want.enabled != cur.enabled {
        p.push(format!(
            "ALTER TASK GRAPH {id} {}",
            if want.enabled { "RESUME" } else { "SUSPEND" }
        ));
    }
    p
}

fn pending(g: &GraphRow) -> bool {
    g.pending
        .as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

async fn may_change(r: Run<'_>, g: &GraphRow) -> Result<()> {
    if let Some(owner) = &g.owner {
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

pub(crate) async fn plan(r: Run<'_>, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let choices = load(r, &["warehouse", "notification_integration"]).await;
    match id {
        None => {
            let s = spec(values, &choices, None)?;
            let taken_sql = format!(
                "SHOW TASK GRAPHS LIKE {} IN SCHEMA {}.{}",
                literal(&s.name),
                quote(&s.database),
                quote(&s.schema)
            );
            let tasks_sql = format!(
                "SHOW TASKS IN SCHEMA {}.{}",
                quote(&s.database),
                quote(&s.schema)
            );
            let (taken, tasks) = tokio::join!(r.rows(&taken_sql), r.rows(&tasks_sql));
            if graph_rows(&taken?).iter().any(|g| g.name == s.name) {
                return Err(bad(format!(
                    "Ya existe un grafo de tareas «{}» en {}.{}.",
                    s.name, s.database, s.schema
                )));
            }
            let tasks = task::shown_rows(&tasks?);
            for (t, l) in [(&s.root, "Tarea raíz"), (&s.finalizer, "Finalizador")] {
                if let Some(t) = t {
                    if !tasks.iter().any(|x| &x.name == t) {
                        return Err(bad(format!(
                            "«{l}»: no existe la tarea {t} en {}.{}.",
                            s.database, s.schema
                        )));
                    }
                }
            }
            if s.enabled && s.root.is_none() {
                return Err(bad(
                    "Un grafo de tareas necesita su tarea raíz para publicarse y activarse.".into(),
                ));
            }
            Ok(create_plan(&s))
        }
        Some(id) => {
            let rd = read(r, id).await?;
            may_change(r, &rd.graph).await?;
            let mut choices = choices;
            for n in [&rd.graph.error_integration, &rd.graph.success_integration]
                .into_iter()
                .flatten()
            {
                choices.keep("notification_integration", n, n);
            }
            if let Some(w) = defaults_of(rd.graph.defaults.as_deref()).warehouse {
                choices.keep("warehouse", &w, &w);
            }
            let want = spec(values, &choices, Some(&rd))?;
            if want.enabled && want.root.is_none() {
                return Err(bad(
                    "Un grafo de tareas necesita su tarea raíz para activarse.".into(),
                ));
            }
            Ok(alter_plan(&current(&rd), &want, pending(&rd.graph)))
        }
    }
}

pub(crate) fn drop_plan(g: &GraphRow) -> Plan {
    let id = g.id();
    let mut p = Plan::new(
        None,
        &format!(
            "Borra el grafo de tareas {}.{}.{} de Snowflake (sus tareas quedan)",
            g.database, g.schema, g.name
        ),
    );
    if g.started {
        p.note("Snowflake borra un grafo de tareas solo suspendido.");
        p.push(format!("ALTER TASK GRAPH {id} SUSPEND"));
    }
    p.push(format!("DROP TASK GRAPH {id}"));
    p
}

pub(crate) async fn delete_plan(r: Run<'_>, id: &str) -> Result<Plan> {
    let rd = read(r, id).await?;
    may_change(r, &rd.graph).await?;
    Ok(drop_plan(&rd.graph))
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn opt(v: &Option<String>) -> Value {
    json!(v.clone().unwrap_or_default())
}

pub(crate) async fn form(r: Run<'_>, id: Option<&str>) -> Result<ObjectForm> {
    let sources: &[&str] = if id.is_none() {
        &["warehouse", "notification_integration", "schema", "task"]
    } else {
        &["warehouse", "notification_integration"]
    };
    let (rd, mut choices) = match id {
        Some(id) => {
            let (a, c) = tokio::join!(read(r, id), load(r, sources));
            (Some(a?), c)
        }
        None => (None, load(r, sources).await),
    };
    let mut f = ObjectForm::default();
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    let existing = rd.is_some();
    match &rd {
        None => {
            values.insert("enabled".into(), json!(false));
            values.insert("commit".into(), json!(true));
            values.insert("overlap".into(), json!("NO_OVERLAP"));
            schedule_values(
                Some(&Schedule::Every(60, super::Unit::Minutes)),
                "none",
                &mut values,
            );
        }
        Some(rd) => {
            let s = current(rd);
            let g = &rd.graph;
            values.insert("name".into(), json!(shown_ident(&g.name)));
            let sid = super::schema_id(&g.database, &g.schema);
            choices.keep("schema", &sid, &format!("{}.{}", g.database, g.schema));
            values.insert("schema".into(), json!(sid));
            values.insert("comment".into(), opt(&g.comment));
            values.insert("enabled".into(), json!(g.started));
            values.insert("commit".into(), json!(true));
            values.insert(
                "pending".into(),
                json!(if pending(g) { "Sí" } else { "No" }),
            );
            values.insert("owner".into(), opt(&g.owner));
            values.insert("created".into(), opt(&g.created_on));
            values.insert("last_suspended".into(), opt(&g.last_suspended_on));
            values.insert("suspended_reason".into(), opt(&g.last_suspended_reason));
            values.insert("sf_id".into(), opt(&g.sf_id));
            schedule_values(s.schedule.as_ref(), "none", &mut values);
            values.insert("condition".into(), opt(&s.condition));
            // The schema's tasks are what the root and the finalizer are
            // picked from.
            let list: Vec<(String, String)> = rd
                .tasks
                .iter()
                .map(|t| (t.id(), format!("{}.{}.{}", t.database, t.schema, t.name)))
                .collect();
            choices.lists.insert("task".into(), list);
            values.insert(
                "root".into(),
                json!(s.root.as_deref().map(|n| s.task_id(n)).unwrap_or_default()),
            );
            values.insert(
                "finalizer".into(),
                json!(s
                    .finalizer
                    .as_deref()
                    .map(|n| s.task_id(n))
                    .unwrap_or_default()),
            );
            values.insert("overlap".into(), json!(s.overlap));
            values.insert("config".into(), opt(&s.config));
            values.insert("auto_retry".into(), json!(""));
            values.insert("suspend_after".into(), json!(""));
            for (k, n) in [
                ("error_integration", &s.error_integration),
                ("success_integration", &s.success_integration),
            ] {
                values.insert(k.into(), opt(n));
                if let Some(n) = n {
                    choices.keep("notification_integration", n, n);
                }
            }
            let d = &s.defaults;
            values.insert("d_warehouse".into(), opt(&d.warehouse));
            if let Some(w) = &d.warehouse {
                choices.keep("warehouse", w, w);
            }
            values.insert("d_initial_size".into(), opt(&d.initial_size));
            values.insert("d_min_size".into(), opt(&d.min_size));
            values.insert("d_max_size".into(), opt(&d.max_size));
            values.insert("d_target_completion".into(), opt(&d.target_completion));
            values.insert(
                "d_timeout_ms".into(),
                d.timeout_ms.map_or(json!(""), |n| json!(n)),
            );
            values.insert(
                "d_execute_as".into(),
                json!(d.execute_as.as_deref().map(shown_ident).unwrap_or_default()),
            );
            values.insert("d_comment".into(), opt(&d.comment));
            values.insert("d_log_level".into(), opt(&d.log_level));
            values.insert("d_trace_level".into(), opt(&d.trace_level));
        }
    }
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing)
            .help("Sin comillas, Snowflake lo guarda en mayúsculas; entre comillas dobles, tal cual."),
        FormField::new("schema", "Esquema", FormKind::reference("schema", false))
            .required()
            .read_only(existing),
        FormField::new("comment", "Comentario", FormKind::Textarea),
        FormField::new("enabled", "Activo (RESUME)", FormKind::Bool)
            .default(json!(false))
            .help("Corre según su programación o su condición, con la versión publicada."),
        FormField::new("commit", "Publicar los cambios (COMMIT)", FormKind::Bool)
            .default(json!(true))
            .help("Los cambios del grafo y de sus tareas quedan pendientes hasta el COMMIT, que publica también los que hubiera pendientes de antes."),
    ]);
    if existing {
        general = general.fields(vec![
            FormField::new("pending", "Cambios sin publicar", FormKind::Text).read_only(true),
            FormField::new("owner", "Rol propietario", FormKind::Text).read_only(true),
            FormField::new("created", "Creado", FormKind::Text).read_only(true),
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
    let mut sched = vec![FormField::new(
        "schedule",
        "Programación",
        FormKind::select(&[
            ("interval", "Cada cierto intervalo"),
            ("cron", "Expresión cron"),
            (
                "none",
                "Sin programación (por su condición o con EXECUTE TASK GRAPH)",
            ),
        ]),
    )
    .default(json!("interval"))];
    sched.extend(schedule_fields(true));
    sched.push(
        FormField::new("condition", "Condición (WHEN)", FormKind::code("sql"))
            .help("Tiene que dar verdadero para que empiece una ejecución del grafo."),
    );
    let schedule = FormSection::new("schedule", "Programación").fields(sched);
    let tasks = FormSection::new("tasks", "Tareas").fields(vec![
        FormField::new("root", "Tarea raíz", FormKind::reference("task", false))
            .help("Una tarea del mismo esquema; sus hijas (AFTER) forman el grafo. Sin raíz no se puede publicar."),
        FormField::new("finalizer", "Finalizador", FormKind::reference("task", false))
            .help("Una tarea del mismo esquema que corre cuando terminan todas las demás."),
    ]);
    let graph = FormSection::new("graph", "Grafo").fields(vec![
        FormField::new("overlap", "Ejecuciones superpuestas", overlap_select())
            .default(json!("NO_OVERLAP")),
        FormField::new("config", "Configuración (CONFIG)", FormKind::code("json"))
            .help("Un objeto JSON que las tareas leen con SYSTEM$GET_TASK_GRAPH_CONFIG."),
        FormField::new(
            "auto_retry",
            "Reintentos automáticos del grafo",
            FormKind::number(Some(0.0), Some(30.0)),
        )
        .help("TASK_AUTO_RETRY_ATTEMPTS (0 a 30). Snowflake no muestra el valor actual: vacío deja el que tiene."),
        FormField::new(
            "suspend_after",
            "Suspender tras fallos seguidos",
            FormKind::number(Some(0.0), None),
        )
        .help("SUSPEND_TASK_AFTER_NUM_FAILURES. Snowflake no muestra el valor actual: vacío deja el que tiene."),
        FormField::new(
            "error_integration",
            "Integración para los errores",
            FormKind::reference("notification_integration", false),
        ),
        FormField::new(
            "success_integration",
            "Integración para los éxitos",
            FormKind::reference("notification_integration", false),
        ),
    ]);
    let defaults = FormSection::new("defaults", "Valores por defecto de las tareas").fields(vec![
        FormField::new(
            "d_warehouse",
            "Warehouse",
            FormKind::reference("warehouse", false),
        )
        .help("TASK_DEFAULTS: valen para cada tarea del grafo que no fije el suyo."),
        FormField::new("d_initial_size", "Tamaño inicial", size_select()),
        FormField::new("d_min_size", "Tamaño mínimo", size_select()),
        FormField::new("d_max_size", "Tamaño máximo", size_select()),
        FormField::new(
            "d_target_completion",
            "Tiempo objetivo de finalización",
            FormKind::Text,
        )
        .placeholder("10 MINUTES"),
        FormField::new(
            "d_timeout_ms",
            "Tiempo máximo de ejecución (ms)",
            FormKind::number(Some(0.0), Some(604_800_000.0)),
        ),
        FormField::new("d_execute_as", "Ejecutar como usuario", FormKind::Text),
        FormField::new("d_comment", "Comentario", FormKind::Textarea),
        FormField::new("d_log_level", "Nivel de registro", log_select()),
        FormField::new("d_trace_level", "Nivel de trazas", trace_select()),
    ]);
    f.sections = vec![general, schedule, tasks, graph, defaults];
    f.values = values;
    f.choices = choices.lists;
    f.notes.push(super::PREVIEW.into());
    if let Some(rd) = &rd {
        f.notes.push(UNKNOWN_PARAMS.into());
        if pending(&rd.graph) {
            f.notes.push(
                "El grafo tiene cambios sin publicar: un COMMIT al guardar los publica también."
                    .into(),
            );
        }
        if let Some(owner) = &rd.graph.owner {
            let roles = held(r, std::slice::from_ref(owner)).await;
            if !roles.get(owner).copied().unwrap_or(true) {
                f.read_only = true;
                f.notes.insert(0, NOT_OWNER.into());
            }
        }
    }
    Ok(f)
}

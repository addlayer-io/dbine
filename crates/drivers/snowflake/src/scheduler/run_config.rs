//! "Ejecutar con otra configuración": one run of a graph now with a
//! configuration of its own, `EXECUTE TASK <root> USING CONFIG = '<json>'`
//! (`EXECUTE TASK GRAPH <graph> USING CONFIG = …` for a task graph object,
//! and for its root's job, as "Ejecutar ahora" runs them).
//!
//! - Snowflake merges the run's configuration into the graph's CONFIG for
//!   that run only: the fields it gives replace the graph's (inside nested
//!   objects too), the others keep their values (a field left out isn't
//!   removed), the definition doesn't change and later runs use the
//!   graph's own. The form starts from the CONFIG the task (or the task
//!   graph object) has now, read from `SHOW TASKS` / `SHOW TASK GRAPHS`.
//! - A task graph object runs its published version: with changes staged
//!   (`has_pending_changes`), the CONFIG `SHOW TASK GRAPHS` shows may be the
//!   staged one, which Snowflake doesn't merge into, and a graph never
//!   published is published (COMMIT) once, with them, before it runs. The
//!   form says so and the script warns (as "Ejecutar ahora" does).
//! - The value has to be a JSON object; it goes in as a string literal.
//! - Only a standalone task or a graph's root runs (Snowflake refuses a
//!   child): a child whose root the role doesn't see isn't offered the
//!   action, as it isn't offered "Ejecutar ahora", and one asked for by id
//!   is refused before anything runs. Snowflake says whether the role may
//!   run it (OWNERSHIP or OPERATE), as for "Ejecutar ahora".
//! - A run asked for while another is executing starts when that one ends;
//!   a later one asked for meanwhile replaces it (only the last
//!   configuration asked for runs).
//!
//! Everything runs on cloud services (SHOW, EXECUTE TASK): the task's own
//! compute runs it, as any run of the graph.

use super::graph::{self, graph_rows, GraphRow};
use super::task::{self, Shown};
use super::{bad, checked_id, json_object, shown, Kind, Plan, Run};
use crate::jobs::{literal, parse_id};
use crate::monitor::quote;
use dbine_driver::{FormField, FormKind, FormSection, FormValues, ObjectForm, Result};
use serde_json::{json, Value};

/// The action's key (the kinds write it as a literal next to its label,
/// for the message extractor).
pub(crate) const RUN_CONFIG: &str = "run_config";

/// The field's key and label (the label as the designer's CONFIG field).
const FIELD: &str = "config";
const LABEL: &str = "Configuración (CONFIG)";

const MERGES: &str = "Snowflake combina esta configuración con la actual solo para esta ejecución: cada campo que escribas reemplaza el suyo (también dentro de un objeto) y los que borres siguen con su valor actual.";
const AFTER_CURRENT: &str = "Si ya hay una ejecución en curso, esta empieza cuando termine la actual; si mientras tanto pedís otra, Snowflake corre solo la última configuración pedida.";
/// A task graph object with changes staged: what the editor starts from may
/// be the staged CONFIG, and Snowflake merges with the published one.
pub(crate) const STAGED_CONFIG: &str = "El grafo de tareas tiene cambios sin publicar: Snowflake combina esta configuración con la CONFIG publicada, y la que muestra el editor puede ser la pendiente.";
const EMPTY: &str = "«Configuración (CONFIG)» no puede quedar vacía: escribí un objeto JSON ({…}).";

// What the script warns about ("⚠" lines: the app lists them and asks to
// continue), each a whole sentence of its own for the message catalog.

/// The run's configuration over a task's (a standalone task or a root).
pub(crate) const TASK_CONFIG: &str = "Solo esta ejecución usa esta configuración, combinada con la de la tarea: los campos que trae reemplazan los suyos, y la tarea sigue con la que tiene en las ejecuciones siguientes.";
/// The same over a task graph object's.
pub(crate) const GRAPH_CONFIG: &str = "Solo esta ejecución usa esta configuración, combinada con la del grafo de tareas: los campos que trae reemplazan los suyos, y el grafo sigue con la que tiene en las ejecuciones siguientes.";

/// What runs, and the configuration it has now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Target {
    /// What follows `EXECUTE`: `TASK "DB"."S"."N"` or `TASK GRAPH "DB"."S"."G"`.
    pub(crate) what: String,
    /// The script's first line.
    pub(crate) header: String,
    /// The CONFIG it has now.
    pub(crate) config: Option<String>,
    /// A task graph object's CONFIG (not a task's).
    pub(crate) graph: bool,
    /// The task graph object a root's job runs by.
    pub(crate) via: Option<String>,
    /// A task graph object with changes staged: the run uses its published
    /// version (and `config` may be the staged one).
    pub(crate) pending: bool,
}

/// `"DB"."S"."N"` as a script's sentence shows it (`DB.S.N`).
fn dotted(id: &str) -> String {
    match parse_id(id) {
        Some((d, s, n)) => format!("{d}.{s}.{n}"),
        None => id.to_string(),
    }
}

fn graph_target(g: &GraphRow, via: Option<String>) -> Target {
    Target {
        what: format!("TASK GRAPH {}", g.id()),
        header: format!(
            "Ejecuta ahora el grafo de tareas {}.{}.{} de Snowflake (su versión publicada) una vez, con esta configuración",
            g.database, g.schema, g.name
        ),
        config: g.config.clone(),
        graph: true,
        via,
        pending: graph::pending(g),
    }
}

/// A standalone task or a plain graph's root (one that doesn't belong to a
/// task graph object). `alone`: it has no child tasks nor finalizer.
pub(crate) fn task_target(t: &Shown, alone: bool) -> Target {
    let name = format!("{}.{}.{}", t.database, t.schema, t.name);
    Target {
        what: format!("TASK {}", t.id()),
        header: if alone {
            format!("Ejecuta ahora la tarea {name} de Snowflake una vez, con esta configuración")
        } else {
            format!("Ejecuta ahora el grafo de la tarea raíz {name} de Snowflake una vez, con esta configuración")
        },
        config: t.config.clone(),
        graph: false,
        via: None,
        pending: false,
    }
}

/// The task graph object `id`, as `SHOW TASK GRAPHS` lists it.
async fn graph_row(r: Run<'_>, id: &str) -> Result<GraphRow> {
    let (db, schema, name) = checked_id(Kind::Graph, id)?;
    let s = r
        .rows(&format!(
            "SHOW TASK GRAPHS LIKE {} IN SCHEMA {}.{}",
            literal(&name),
            quote(&db),
            quote(&schema)
        ))
        .await?;
    graph_rows(&s)
        .into_iter()
        .find(|g| g.name == name && g.database == db && g.schema == schema)
        .ok_or_else(|| {
            bad(format!(
                "No existe el grafo de tareas {db}.{schema}.{name} o el rol no lo ve."
            ))
        })
}

/// What the action on the object `id` of `kind` runs. Anything else than a
/// root or standalone task (a job) or a task graph object is refused.
async fn target(r: Run<'_>, kind: Kind, id: &str) -> Result<Target> {
    match kind {
        Kind::Graph => Ok(graph_target(&graph_row(r, id).await?, None)),
        Kind::Job => {
            let (db, schema, name) = checked_id(Kind::Job, id)?;
            let tasks = task::schema_tasks(r, &db, &schema).await?;
            let at = tasks
                .iter()
                .position(|t| t.name == name)
                .ok_or_else(|| task::not_found(&db, &schema, &name))?;
            let t = &tasks[at];
            if !t.is_root() {
                return Err(bad(format!(
                    "Snowflake ejecuta solo una tarea suelta o la raíz de un grafo, y «{}» es una tarea hija.",
                    shown(&name)
                )));
            }
            match &t.graph {
                // The root of a task graph object runs by the object, with
                // the object's configuration ("Ejecutar ahora" does the same).
                Some(obj) => Ok(graph_target(&graph_row(r, obj).await?, Some(obj.clone()))),
                None => Ok(task_target(t, task::members_of(&tasks, at).is_empty())),
            }
        }
        Kind::Alert => Err(bad(
            "Las alertas de Snowflake no se ejecutan con otra configuración.".into(),
        )),
    }
}

/// The form: the configuration, starting from the current one.
pub(crate) fn form_of(t: &Target) -> ObjectForm {
    let now = t
        .config
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or("{}");
    let mut f = ObjectForm {
        sections: vec![FormSection::new("run", "Ejecución").fields(vec![FormField::new(
            "config",
            "Configuración (CONFIG)",
            FormKind::code("json"),
        )
        .required()
        .help("Un objeto JSON, que las tareas leen con SYSTEM$GET_TASK_GRAPH_CONFIG. Viene con la configuración actual.")])],
        ..ObjectForm::default()
    };
    f.values.insert(FIELD.into(), json!(now));
    f.notes.push(MERGES.into());
    if let Some(obj) = &t.via {
        f.notes.push(format!(
            "La tarea es la raíz del grafo de tareas {}: la configuración es la del grafo.",
            dotted(obj)
        ));
    }
    if t.pending {
        f.notes.push(STAGED_CONFIG.into());
    }
    f.notes.push(AFTER_CURRENT.into());
    f
}

/// The form's configuration: a JSON object, as typed (an object sent as
/// JSON, not as text, is taken too).
pub(crate) fn config_of(values: &FormValues) -> Result<String> {
    let text = match values.values.get(FIELD) {
        Some(Value::String(s)) => s.clone(),
        Some(v @ Value::Object(_)) => v.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    };
    json_object(&text, LABEL)?.ok_or_else(|| bad(EMPTY.into()))
}

/// The statement and its script for `values`.
pub(crate) fn plan_of(t: &Target, values: &FormValues) -> Result<Plan> {
    let config = config_of(values)?;
    let mut p = Plan::new(None, &t.header);
    p.warn(if t.graph { GRAPH_CONFIG } else { TASK_CONFIG });
    if t.pending {
        p.warn(graph::RUNS_PUBLISHED);
    }
    p.push(format!(
        "EXECUTE {} USING CONFIG = {}",
        t.what,
        literal(&config)
    ));
    Ok(p)
}

pub(crate) async fn form(r: Run<'_>, kind: Kind, id: &str) -> Result<ObjectForm> {
    Ok(form_of(&target(r, kind, id).await?))
}

pub(crate) async fn plan(r: Run<'_>, kind: Kind, id: &str, values: &FormValues) -> Result<Plan> {
    // The values first: a bad one is refused before anything is read.
    config_of(values)?;
    plan_of(&target(r, kind, id).await?, values)
}

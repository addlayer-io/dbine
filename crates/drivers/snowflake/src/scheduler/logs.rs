//! The scheduler's log (the alerts' runs and the tasks' failed runs of the
//! last 7 days), the start of a graph at one of its tasks and the history
//! Snowflake doesn't let anyone clear.
//!
//! The history is `INFORMATION_SCHEMA.ALERT_HISTORY` / `TASK_HISTORY`,
//! which need a running warehouse: as the jobs code, a suspended one is
//! never resumed to read them.

use super::task::{self, Graph};
use super::{bad, checked_id, running_warehouse, shown, Kind, Run};
use crate::jobs::{self, get, literal, text};
use crate::monitor::quote;
use dbine_driver::{Error, MonitorTable, Result};
use serde_json::json;
use std::collections::HashMap;

/// Rows of each log table, at most.
const LOG_ROWS: usize = 1000;
/// The history's window: 7 days minus an hour (the functions refuse a start
/// older than 7 days).
const WINDOW: &str = "DATEADD('hour', -167, CURRENT_TIMESTAMP())";

const NO_CLEAR: &str = "Snowflake no permite borrar el historial de las tareas: lo guarda él (7 días en INFORMATION_SCHEMA.TASK_HISTORY y un año en ACCOUNT_USAGE.TASK_HISTORY).";

/// A database whose `INFORMATION_SCHEMA` serves the account's history: the
/// session's, or the first the role sees.
async fn some_database(r: Run<'_>) -> Result<String> {
    if let Some(db) = r.database() {
        return Ok(db);
    }
    let s = r.rows("SHOW DATABASES LIMIT 1").await?;
    text(&s, 0, "name").ok_or_else(|| {
        bad("Para leer el historial del planificador hace falta una base de datos visible.".into())
    })
}

fn alerts_sql(db: &str) -> String {
    format!(
        "SELECT TO_VARCHAR(scheduled_time, 'YYYY-MM-DD HH24:MI:SS TZH:TZM') AS scheduled,
       TO_VARCHAR(completed_time, 'YYYY-MM-DD HH24:MI:SS TZH:TZM') AS completed,
       database_name, schema_name, name, state, scheduled_from, sql_error_code, sql_error_message
  FROM TABLE({}.INFORMATION_SCHEMA.ALERT_HISTORY(
         SCHEDULED_TIME_RANGE_START => {WINDOW}, RESULT_LIMIT => {LOG_ROWS}))
 WHERE state <> 'SCHEDULED'
 ORDER BY scheduled_time DESC",
        quote(db)
    )
}

fn tasks_sql(db: &str) -> String {
    format!(
        "SELECT TO_VARCHAR(scheduled_time, 'YYYY-MM-DD HH24:MI:SS TZH:TZM') AS scheduled,
       TO_VARCHAR(completed_time, 'YYYY-MM-DD HH24:MI:SS TZH:TZM') AS completed,
       database_name, schema_name, name, state, scheduled_from, error_code AS sql_error_code,
       error_message AS sql_error_message
  FROM TABLE({}.INFORMATION_SCHEMA.TASK_HISTORY(
         SCHEDULED_TIME_RANGE_START => {WINDOW}, RESULT_LIMIT => {LOG_ROWS}, ERROR_ONLY => TRUE))
 ORDER BY scheduled_time DESC",
        quote(db)
    )
}

fn state_label(state: &str) -> String {
    match state.to_ascii_uppercase().as_str() {
        "SUCCEEDED" => "Correcta".into(),
        "FAILED" => "Fallida".into(),
        "FAILED_AND_AUTO_SUSPENDED" => "Fallida (suspendida por fallos)".into(),
        "CANCELLED" | "CANCELED" => "Cancelada".into(),
        "SKIPPED" => "Salteada".into(),
        "EXECUTING" => "En curso".into(),
        "CONDITION_FALSE" => "Condición falsa".into(),
        "CONDITION_FAILED" => "Falló la condición".into(),
        "ACTION_FAILED" => "Falló la acción".into(),
        "TRIGGERED" => "Disparada".into(),
        _ => state.to_string(),
    }
}

fn origin_label(from: &str) -> String {
    match from.to_ascii_uppercase().as_str() {
        "SCHEDULE" => "programada".into(),
        "EXECUTE TASK" | "EXECUTE ALERT" => "manual".into(),
        "TRIGGER" => "datos nuevos".into(),
        _ => from.to_string(),
    }
}

/// A history's rows as a table.
pub(crate) fn table(key: &str, title: &str, s: &crate::monitor::Set) -> MonitorTable {
    let mut t = MonitorTable::new(
        "scheduler_log",
        "Registro del planificador",
        &[
            "Programada",
            "Terminada",
            "Objeto",
            "Estado",
            "Origen",
            "Mensaje",
        ],
    );
    t.key = key.to_string();
    t.title = title.to_string();
    t.rows = (0..s.rows.len())
        .map(|i| {
            let object = [
                text(s, i, "database_name"),
                text(s, i, "schema_name"),
                text(s, i, "name"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(".");
            let message = match (get(s, i, "sql_error_code"), get(s, i, "sql_error_message")) {
                (Some(c), Some(m)) => format!("{m} ({c})"),
                (_, Some(m)) => m.to_string(),
                _ => String::new(),
            };
            vec![
                json!(text(s, i, "scheduled").unwrap_or_default()),
                json!(text(s, i, "completed").unwrap_or_default()),
                json!(object),
                json!(state_label(get(s, i, "state").unwrap_or_default())),
                json!(origin_label(
                    get(s, i, "scheduled_from").unwrap_or_default()
                )),
                json!(message),
            ]
        })
        .collect();
    t
}

/// A table that says why a history couldn't be read.
fn failed(key: &str, title: &str, e: &Error) -> MonitorTable {
    let mut t = MonitorTable::new("scheduler_log", "Registro del planificador", &["Mensaje"]);
    t.key = key.to_string();
    t.title = title.to_string();
    t.rows = vec![vec![json!(format!("No se pudo leer: {e}"))]];
    t
}

pub(crate) async fn logs(r: Run<'_>) -> Result<Vec<MonitorTable>> {
    running_warehouse(r, "leer el registro del planificador").await?;
    let db = some_database(r).await?;
    let (a_sql, t_sql) = (alerts_sql(&db), tasks_sql(&db));
    let (alerts, tasks) = tokio::join!(r.rows(&a_sql), r.rows(&t_sql));
    if let (Err(a), Err(_)) = (&alerts, &tasks) {
        return Err(bad(format!(
            "No se pudo leer el historial del planificador: {a}"
        )));
    }
    let mut out = Vec::new();
    match &alerts {
        Ok(s) => out.push(table("alert_history", "Ejecuciones de alertas", s)),
        Err(e) => out.push(failed("alert_history", "Ejecuciones de alertas", e)),
    }
    match &tasks {
        Ok(s) => out.push(table("task_errors", "Tareas con errores", s)),
        Err(e) => out.push(failed("task_errors", "Tareas con errores", e)),
    }
    Ok(out)
}

/// `TASK_HISTORY` is Snowflake's: nothing deletes it.
pub(crate) async fn clear_history(_r: Run<'_>, id: &str) -> Result<()> {
    checked_id(Kind::Job, id)?;
    Err(Error::Unsupported(NO_CLEAR.into()))
}

/// The last graph run's tasks, from `TASK_HISTORY` (`ROOT_TASK_ID`, or
/// `GRAPH_ID` for a task graph object): each task's latest attempt.
fn last_run_sql(db: &str, by: &str, sf_id: &str) -> String {
    format!(
        "SELECT name, state, graph_run_group_id, attempt_number,
       DATE_PART(EPOCH_MILLISECOND, scheduled_time) AS scheduled_ms
  FROM TABLE({}.INFORMATION_SCHEMA.TASK_HISTORY(
         SCHEDULED_TIME_RANGE_START => {WINDOW}, RESULT_LIMIT => 10000, {by} => {}))
 WHERE state <> 'SCHEDULED'",
        quote(db),
        literal(sf_id)
    )
}

/// How each task of the graph's latest run ended (its latest attempt), and
/// whether any runs still.
pub(crate) fn last_run(s: &crate::monitor::Set, root: &str) -> HashMap<String, String> {
    // The latest graph run: the group of the root's newest run.
    let mut newest: Option<(i64, String)> = None;
    for i in 0..s.rows.len() {
        let (Some(n), Some(g)) = (get(s, i, "name"), text(s, i, "graph_run_group_id")) else {
            continue;
        };
        let at = get(s, i, "scheduled_ms")
            .and_then(|v| v.parse::<f64>().ok())
            .map_or(0, |f| f as i64);
        if n == root && newest.as_ref().is_none_or(|(t, _)| at > *t) {
            newest = Some((at, g));
        }
    }
    let Some((_, group)) = newest else {
        return HashMap::new();
    };
    let mut best: HashMap<String, (i64, String)> = HashMap::new();
    for i in 0..s.rows.len() {
        if get(s, i, "graph_run_group_id") != Some(group.as_str()) {
            continue;
        }
        let (Some(n), Some(state)) = (text(s, i, "name"), text(s, i, "state")) else {
            continue;
        };
        let attempt = get(s, i, "attempt_number")
            .and_then(|v| v.parse::<f64>().ok())
            .map_or(0, |f| f as i64);
        if best.get(&n).is_none_or(|(a, _)| attempt >= *a) {
            best.insert(n, (attempt, state.to_ascii_uppercase()));
        }
    }
    best.into_iter().map(|(k, (_, s))| (k, s)).collect()
}

/// What starting at `step` runs: the root's name runs the whole graph
/// (`EXECUTE TASK`); another task, the retry of the last run from the tasks
/// that failed (`RETRY LAST`), when that one is among them.
pub(crate) fn run_from_sql(
    g: &Graph,
    step: &str,
    last: Option<&HashMap<String, String>>,
) -> Result<String> {
    let target = match &g.root.graph {
        Some(obj) => format!("TASK GRAPH {obj}"),
        None => format!("TASK {}", g.root.id()),
    };
    let step_name = if step == g.root.name {
        return Ok(format!("EXECUTE {target}"));
    } else if let Some((t, _)) = g.members.iter().find(|(t, _)| t.name == step) {
        t.name.clone()
    } else {
        return Err(bad(format!(
            "El grafo no tiene una tarea llamada «{}».",
            shown(step)
        )));
    };
    let Some(last) = last else {
        return Err(bad(format!(
            "Snowflake empieza un grafo solo por su raíz; desde «{step_name}» solo puede reintentar la última ejecución si falló en esa tarea, y para saberlo hace falta leer el historial."
        )));
    };
    if last.values().any(|s| s == "EXECUTING") {
        return Err(bad(
            "El grafo se está ejecutando: Snowflake reintenta solo una ejecución terminada.".into(),
        ));
    }
    match last.get(&step_name).map(String::as_str) {
        Some("FAILED" | "CANCELLED" | "CANCELED" | "FAILED_AND_AUTO_SUSPENDED") => {
            Ok(format!("EXECUTE {target} RETRY LAST"))
        }
        other => Err(bad(format!(
            "Snowflake empieza un grafo solo por su raíz; desde «{step_name}» solo reintenta la última ejecución (RETRY LAST) cuando falló o se canceló en esa tarea, y {}.",
            match other {
                Some(s) => format!("en la última terminó «{}»", state_label(s)),
                None => "la última no llegó a esa tarea".to_string(),
            }
        ))),
    }
}

/// "Ejecutar ahora" of the jobs list: the job's whole graph, by the same
/// statement as the start at its root ([`run_from_sql`]: `EXECUTE TASK`, or
/// `EXECUTE TASK GRAPH` for the root of a task graph object). One `SHOW
/// TASKS` of the schema (cloud services) tells which; a task the role
/// doesn't see there is run as named, and Snowflake answers.
pub(crate) async fn run_now(r: Run<'_>, db: &str, schema: &str, name: &str) -> Result<()> {
    let root = task::schema_tasks(r, db, schema)
        .await?
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| task::Shown {
            database: db.to_string(),
            schema: schema.to_string(),
            name: name.to_string(),
            ..task::Shown::default()
        });
    let g = Graph {
        root,
        ..Graph::default()
    };
    let sql = run_from_sql(&g, &g.root.name, None)?;
    r.rows(&sql).await.map(|_| ())
}

pub(crate) async fn run_from(r: Run<'_>, id: &str, step: &str) -> Result<()> {
    let g = task::read(r, id).await?;
    if g.orphan {
        return Err(bad(
            "Es una tarea hija cuya raíz el rol no ve: Snowflake la ejecuta solo con su grafo."
                .into(),
        ));
    }
    let sql = if step == g.root.name {
        run_from_sql(&g, step, None)?
    } else {
        if !g.members.iter().any(|(t, _)| t.name == step) {
            return Err(bad(format!(
                "El grafo no tiene una tarea llamada «{}».",
                shown(step)
            )));
        }
        running_warehouse(r, "ver en qué tarea falló el grafo").await?;
        let (by, sf_id) = match &g.root.graph {
            Some(obj) => {
                let (db, schema, name) = jobs::parse_id(obj)
                    .ok_or_else(|| bad(format!("«{obj}» no es un grafo de tareas")))?;
                let s = r
                    .rows(&format!(
                        "SHOW TASK GRAPHS LIKE {} IN SCHEMA {}.{}",
                        literal(&name),
                        quote(&db),
                        quote(&schema)
                    ))
                    .await?;
                let id = super::graph::graph_rows(&s)
                    .into_iter()
                    .find(|x| x.name == name)
                    .and_then(|x| x.sf_id)
                    .ok_or_else(|| bad(format!("No se encontró el grafo de tareas {obj}.")))?;
                ("GRAPH_ID", id)
            }
            None => (
                "ROOT_TASK_ID",
                g.root
                    .sf_id
                    .clone()
                    .ok_or_else(|| bad("Snowflake no informó el id de la tarea raíz.".into()))?,
            ),
        };
        let s = r.rows(&last_run_sql(&g.root.database, by, &sf_id)).await?;
        let last = last_run(&s, &g.root.name);
        run_from_sql(&g, step, Some(&last))?
    };
    r.rows(&sql).await.map(|_| ())
}

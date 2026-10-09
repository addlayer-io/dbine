//! "Tareas programadas" (docs/scheduled-tasks.md): the list, editing,
//! the OS scheduler entry of each task, "Ejecutar ahora" and the history.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use crate::tasks::{self, WriteScope};
use dbine_core::tasks::{kinds, ScheduledTask, TaskRun};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use tauri::{AppHandle, Emitter, State};

#[derive(Serialize)]
pub struct TaskItem {
    #[serde(flatten)]
    pub task: ScheduledTask,
    /// The OS scheduler has its entry.
    pub registered: bool,
    pub next_run: Option<String>,
    pub last_run: Option<TaskRun>,
    /// Steps that write, approved or not.
    pub writes: Vec<WriteScope>,
    /// They changed since the approval (the task won't run them).
    pub needs_approval: bool,
}

fn item(state: &AppState, task: ScheduledTask) -> TaskItem {
    let now = chrono::Local::now().naive_local();
    let fingerprint = tasks::write_fingerprint(state, &task);
    TaskItem {
        registered: tasks::os::is_registered(&task.id),
        next_run: task.enabled.then(|| task.schedule.next_after(now)).flatten().map(|t| t.format("%Y-%m-%d %H:%M").to_string()),
        last_run: state.store.list_task_runs(Some(&task.id), 1).ok().and_then(|r| r.into_iter().next()),
        writes: tasks::write_scope(state, &task),
        needs_approval: fingerprint.is_some() && fingerprint != task.approved_writes,
        task,
    }
}

#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_tasks_list(state: State<'_, AppState>) -> CommandResult<Vec<TaskItem>> {
    Ok(state.store.list_tasks()?.into_iter().map(|t| item(&state, t)).collect())
}

#[derive(Deserialize)]
pub struct CheckArgs {
    pub task: ScheduledTask,
}

#[derive(Serialize)]
pub struct Check {
    /// The steps that write.
    pub scope: Vec<WriteScope>,
    /// They aren't what was approved: saving asks for the approval.
    pub needs_approval: bool,
}

/// What saving `task` would ask the user to approve.
#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_task_check(state: State<'_, AppState>, args: CheckArgs) -> CommandResult<Check> {
    let fingerprint = tasks::write_fingerprint(&state, &args.task);
    let needs_approval = fingerprint.is_some() && fingerprint != args.task.approved_writes;
    Ok(Check { scope: tasks::write_scope(&state, &args.task), needs_approval })
}

#[derive(Deserialize)]
pub struct SaveArgs {
    pub task: ScheduledTask,
    /// The user approved the steps that write, as `scheduled_task_check`
    /// listed them.
    #[serde(default)]
    pub approve_writes: bool,
}

#[derive(Serialize)]
pub struct Saved {
    pub item: TaskItem,
    /// Saved, but the OS scheduler refused it (the task won't run by itself).
    pub schedule_error: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_task_save(state: State<'_, AppState>, args: SaveArgs) -> CommandResult<Saved> {
    let mut task = args.task;
    if task.id.is_empty() {
        task.id = uuid::Uuid::new_v4().to_string();
    }
    for step in task.steps.iter_mut().filter(|s| s.id.is_empty()) {
        step.id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    }
    if let Some(problem) = task.problem() {
        return Err(CommandError::BadRequest(problem));
    }
    let fingerprint = tasks::write_fingerprint(&state, &task);
    match (&fingerprint, args.approve_writes) {
        (None, _) => task.approved_writes = None,
        (Some(f), true) => task.approved_writes = Some(f.clone()),
        // Unchanged since the last approval: it stands.
        (Some(f), false) if task.approved_writes.as_ref() == Some(f) => {}
        (Some(_), false) => {
            return Err(CommandError::BadRequest("la tarea cambia datos o estructura: hay que aprobar esos pasos para guardarla".into()));
        }
    }
    keep_secrets_apart(&state, &mut task)?;
    let saved = state.store.save_task(&task)?;
    let schedule_error = tasks::os::sync(&saved).err();
    if let Some(e) = &schedule_error {
        tracing::warn!(task = %saved.id, "scheduled task not registered: {e}");
    }
    Ok(Saved { item: item(&state, saved), schedule_error })
}

/// A backup step's secret options go to the vault; the task keeps them
/// empty. An empty value with a secret already kept means "unchanged".
fn keep_secrets_apart(state: &AppState, task: &mut ScheduledTask) -> CommandResult<()> {
    for step in task.steps.iter_mut().filter(|s| s.kind == kinds::BACKUP) {
        let conn = step.config.get("connection_id").and_then(Value::as_str).unwrap_or("").to_string();
        let keys = tasks::secret_option_keys(&conn, state);
        let name = tasks::step_secret_name(&task.id, &step.id);
        let mut kept: BTreeMap<String, String> =
            dbine_core::secrets::get_raw(&name).ok().flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let Some(options) = step.config.get_mut("options").and_then(Value::as_object_mut) else { continue };
        for key in keys {
            if let Some(v) = options.get_mut(key) {
                let typed = v.as_str().unwrap_or("").to_string();
                if !typed.is_empty() {
                    kept.insert(key.to_string(), typed);
                }
                *v = Value::String(String::new());
            }
        }
        if kept.is_empty() {
            let _ = dbine_core::secrets::delete_raw(&name);
        } else {
            let json = serde_json::to_string(&kept).map_err(|e| CommandError::Internal(e.to_string()))?;
            dbine_core::secrets::set_raw(&name, &json)?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_task_delete(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    if let Ok(task) = tasks::load(&state, &args.id) {
        for step in &task.steps {
            let _ = dbine_core::secrets::delete_raw(&tasks::step_secret_name(&task.id, &step.id));
        }
    }
    tasks::os::unregister(&args.id).map_err(CommandError::Internal)?;
    state.store.delete_task(&args.id)?;
    Ok(())
}

#[derive(Deserialize)]
pub struct EnableArgs {
    pub id: String,
    pub enabled: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_task_enable(state: State<'_, AppState>, args: EnableArgs) -> CommandResult<Saved> {
    let mut task = tasks::load(&state, &args.id)?;
    task.enabled = args.enabled;
    let saved = state.store.save_task(&task)?;
    let schedule_error = tasks::os::sync(&saved).err();
    Ok(Saved { item: item(&state, saved), schedule_error })
}

/// "Ejecutar ahora": runs in the app, in the background; `task-run-finished`
/// tells the windows (the history updates with `state-changed`).
#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_task_run_now(app: AppHandle, state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    let task = tasks::load(&state, &args.id)?;
    let state = state.inner().clone();
    tauri::async_runtime::spawn(async move {
        let run = tasks::run_task(&state, Some(&app), &task, "manual").await;
        let _ = app.emit("task-run-finished", &run);
    });
    Ok(())
}

#[derive(Deserialize)]
pub struct RunsArgs {
    pub task_id: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_task_runs(state: State<'_, AppState>, args: RunsArgs) -> CommandResult<Vec<TaskRun>> {
    Ok(state.store.list_task_runs(args.task_id.as_deref(), args.limit.unwrap_or(50).min(500))?)
}

/// The OS scheduler entries match the list (after a restore, a move to a
/// new folder, or an update that changed the app's path).
#[cfg_attr(debug_assertions, allow(dead_code))]
pub fn resync_all(state: &AppState) {
    let Ok(list) = state.store.list_tasks() else { return };
    for task in list.iter().filter(|t| t.enabled) {
        if let Err(e) = tasks::os::sync(task) {
            tracing::warn!(task = %task.id, "scheduled task not registered: {e}");
        }
    }
}

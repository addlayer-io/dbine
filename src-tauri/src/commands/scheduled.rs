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
    /// What `scope` is the approval of: the save takes it back with the
    /// user's yes ([`SaveArgs::approved_fingerprint`]).
    pub fingerprint: Option<String>,
}

/// What saving `task` would ask the user to approve.
#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_task_check(state: State<'_, AppState>, args: CheckArgs) -> CommandResult<Check> {
    // The scope and its fingerprint from the same read of the notebooks.
    let scope = tasks::write_scope(&state, &args.task);
    let fingerprint = tasks::task_fingerprint(&scope, &args.task);
    let needs_approval = fingerprint.is_some() && fingerprint != args.task.approved_writes;
    Ok(Check { scope, needs_approval, fingerprint })
}

#[derive(Deserialize)]
pub struct SaveArgs {
    pub task: ScheduledTask,
    /// The user approved the steps that write, as `scheduled_task_check`
    /// listed them.
    #[serde(default)]
    pub approve_writes: bool,
    /// The fingerprint of what the user approved (`Check::fingerprint`).
    #[serde(default)]
    pub approved_fingerprint: Option<String>,
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
    // The ids the steps had when the user saw the approval (a new one's is empty).
    let ids_seen: Vec<String> = task.steps.iter().map(|s| s.id.clone()).collect();
    if task.id.is_empty() {
        task.id = uuid::Uuid::new_v4().to_string();
    }
    for step in task.steps.iter_mut().filter(|s| s.id.is_empty()) {
        step.id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    }
    if let Some(problem) = task.problem() {
        return Err(CommandError::BadRequest(problem));
    }
    let approved = args.approve_writes.then_some(args.approved_fingerprint.as_deref()).flatten();
    approve_writes(&state, &mut task, &ids_seen, approved)?;
    keep_secrets_apart(&state, &mut task)?;
    // A step that was removed takes its kept secrets with it.
    let removed = forgotten_secrets(tasks::load(&state, &task.id).ok().as_ref(), Some(&task));
    let saved = state.store.save_task(&task)?;
    for name in removed {
        let _ = dbine_core::secrets::delete_raw(&name);
    }
    let schedule_error = tasks::os::sync(&saved).err();
    if let Some(e) = &schedule_error {
        tracing::warn!(task = %saved.id, "scheduled task not registered: {e}");
    }
    Ok(Saved { item: item(&state, saved), schedule_error })
}

/// Sets `task.approved_writes` for the save, or refuses it ([`approval`]).
/// `ids_seen`: the steps' ids when the user saw the check.
fn approve_writes(state: &AppState, task: &mut ScheduledTask, ids_seen: &[String], approved: Option<&str>) -> CommandResult<()> {
    let scope = tasks::write_scope(state, task);
    let fingerprint = tasks::task_fingerprint(&scope, task);
    // The same read, as `scheduled_task_check` fingerprinted it: before the
    // new steps got their ids.
    let seen = tasks::task_fingerprint(
        &scope
            .into_iter()
            .map(|mut w| {
                if let Some(n) = task.steps.iter().position(|s| s.id == w.step_id) {
                    w.step_id = ids_seen.get(n).cloned().unwrap_or_default();
                }
                w
            })
            .collect::<Vec<_>>(),
        task,
    );
    task.approved_writes = approval(fingerprint, seen.as_deref(), approved, task.approved_writes.as_deref())?;
    Ok(())
}

/// The approval the task is saved with. `now`: the fingerprint of what
/// writes, read now; `seen`: the same, under the step ids the check had;
/// `approved`: the one the user said yes to (what `scheduled_task_check`
/// showed); `standing`: the task's previous one. A yes to something else
/// (a notebook edited between the check and the save) approves nothing:
/// the user is asked again.
fn approval(now: Option<String>, seen: Option<&str>, approved: Option<&str>, standing: Option<&str>) -> CommandResult<Option<String>> {
    match now {
        None => Ok(None),
        Some(f) if approved.is_some() && approved == seen => Ok(Some(f)),
        // Unchanged since the last approval: it stands.
        Some(f) if standing == Some(f.as_str()) => Ok(Some(f)),
        Some(_) if approved.is_some() => Err(CommandError::BadRequest(
            "lo que la tarea cambia ya no es lo que se aprobó (un archivo cambió mientras tanto): hay que revisarlo y aprobarlo de nuevo".into(),
        )),
        Some(_) => Err(CommandError::BadRequest("la tarea cambia datos o estructura: hay que aprobar esos pasos para guardarla".into())),
    }
}

/// The vault entries of the steps in `before` that are not in `after` (all
/// of them when the task is deleted: `after` is `None`).
fn forgotten_secrets(before: Option<&ScheduledTask>, after: Option<&ScheduledTask>) -> Vec<String> {
    let Some(before) = before else { return Vec::new() };
    before
        .steps
        .iter()
        .filter(|b| !after.is_some_and(|a| a.steps.iter().any(|s| s.id == b.id)))
        .map(|b| tasks::step_secret_name(&before.id, &b.id))
        .collect()
}

/// A backup step's secret options, a notebook step's credential parameters
/// and a message step's webhook and tokens go to the vault; the task keeps
/// them empty (a notebook step's, not at all). An empty value with a secret
/// already kept means "unchanged".
fn keep_secrets_apart(state: &AppState, task: &mut ScheduledTask) -> CommandResult<()> {
    let messages = tasks::message::secrets_apart(task, &|n| dbine_core::secrets::get_raw(n).ok().flatten()).map_err(CommandError::BadRequest)?;
    for (name, kept) in messages.into_iter().chain(tasks::notebook_secrets_apart(task, &|n| dbine_core::secrets::get_raw(n).ok().flatten())) {
        match kept {
            Some(json) => dbine_core::secrets::set_raw(&name, &json)?,
            None => {
                let _ = dbine_core::secrets::delete_raw(&name);
            }
        }
    }
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
        for name in forgotten_secrets(Some(&task), None) {
            let _ = dbine_core::secrets::delete_raw(&name);
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
    // Entries under the old identifier's label (macOS) move first.
    let moved = tasks::os::migrate_old_labels(&list);
    for task in list.iter().filter(|t| t.enabled && !moved.contains(&t.id)) {
        if let Err(e) = tasks::os::sync(task) {
            tracing::warn!(task = %task.id, "scheduled task not registered: {e}");
        }
    }
}

/// What the "Ejecutar notebook" step's editor shows for a notebook: where
/// each cell runs here, the targets that need "Permitir ejecutar … sin
/// confirmar", the files it reads (and which lack "Permitir siempre"), its
/// write cells and parameters. Reads the file; runs nothing.
#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_notebook_plan(app: AppHandle, state: State<'_, AppState>, args: crate::notebook::headless::Options) -> CommandResult<crate::notebook::headless::Plan> {
    let mut dirs = crate::notebook::headless::own_dirs();
    dirs.extend(crate::commands::notebook::app_dirs(&app));
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || crate::notebook::headless::plan(&state, &args, crate::notebook::access::Policy::current(&dirs)))
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
}

#[derive(Deserialize)]
pub struct MessageTestArgs {
    /// The task's id ("" before its first save) and name.
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub task_name: String,
    /// The step as it is in the editor (typed secrets, or empty for the
    /// saved ones of the same channel).
    pub step: dbine_core::tasks::Step,
    /// The test message, in the user's language.
    pub text: String,
}

/// "Probar" in an "Enviar un mensaje" step: sends a test message now. The
/// error never carries the webhook or the tokens.
#[tauri::command(rename_all = "camelCase")]
pub async fn scheduled_message_test(args: MessageTestArgs) -> CommandResult<String> {
    if args.step.kind != kinds::SEND_MESSAGE {
        return Err(CommandError::BadRequest("el paso no envía mensajes".into()));
    }
    tasks::message::test(&args.task_id, &args.task_name, &args.step, &args.text).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A removed step's secrets (a message step's webhook…) leave the vault
    /// with it; deleting the task takes all of them.
    #[test]
    fn removed_steps_and_tasks_forget_their_secrets() {
        use dbine_core::tasks::Step;
        let step = |id: &str| Step { id: id.into(), kind: kinds::SEND_MESSAGE.into(), ..Default::default() };
        let before = ScheduledTask { id: "t".into(), steps: vec![step("a"), step("b")], ..Default::default() };
        let after = ScheduledTask { id: "t".into(), steps: vec![step("b")], ..Default::default() };
        assert_eq!(forgotten_secrets(Some(&before), Some(&after)), vec!["task-secret:t:a"]);
        assert_eq!(forgotten_secrets(Some(&before), None), vec!["task-secret:t:a", "task-secret:t:b"]);
        assert!(forgotten_secrets(None, Some(&after)).is_empty());
    }

    #[test]
    fn a_save_approves_only_what_the_user_saw() {
        let f = |s: &str| Some(s.to_string());
        // Nothing writes: no approval.
        assert_eq!(approval(None, None, Some("a"), None).unwrap(), None);
        // The yes is to what is there now (`seen`: under the check's step
        // ids; `now`: the ids it is saved with).
        assert_eq!(approval(f("a2"), Some("a"), Some("a"), None).unwrap(), f("a2"));
        // What writes changed after the check (the notebook was edited):
        // the yes was to something else, so it is asked again.
        let e = approval(f("b"), Some("b"), Some("a"), None).unwrap_err();
        assert!(e.to_string().contains("ya no es lo que se aprobó"), "{e}");
        assert!(approval(f("b"), Some("b"), Some("a"), Some("old")).is_err());
        // Back to what was approved before: that approval stands.
        assert_eq!(approval(f("b"), Some("b"), Some("a"), Some("b")).unwrap(), f("b"));
        // No yes: only an approval that still stands.
        assert_eq!(approval(f("a"), Some("a"), None, Some("a")).unwrap(), f("a"));
        assert!(approval(f("a"), Some("a"), None, Some("old")).is_err());
        assert!(approval(f("a"), Some("a"), None, None).is_err());
    }

    /// The check's fingerprint is the one the save compares the yes with,
    /// new steps (no id yet) included; a script changed after the check is
    /// not approved by it.
    #[test]
    fn the_checked_fingerprint_is_what_the_save_takes() {
        use dbine_core::state::SavedConnection;
        use dbine_core::tasks::{kinds, Step};
        let dir = std::env::temp_dir().join(format!("dbine-sched-approve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.sqlite");
        rusqlite::Connection::open(&file).unwrap().execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER);").unwrap();
        let state = AppState::new(dbine_core::StateStore::open_in_memory().unwrap());
        let config = dbine_driver::ConnectionConfig { driver: "sqlite".into(), host: file.to_string_lossy().into_owned(), ..Default::default() };
        let conn = SavedConnection { id: "a".into(), name: "a".into(), color: None, config, save_password: false, folder_id: None, tags: vec![], mcp_level: None, updated_at: String::new() };
        state.store.save_connection(&conn).unwrap();
        let step = |sql: &str| Step { kind: kinds::RUN_SCRIPT.into(), config: serde_json::json!({"connection_id": "a", "sql": sql}), ..Default::default() };
        let new_task = |sql: &str| ScheduledTask { name: "t".into(), steps: vec![step(sql)], ..Default::default() };

        // What the UI checked, and the user said yes to.
        let checked = tasks::fingerprint_of(&tasks::write_scope(&state, &new_task("DELETE FROM t"))).unwrap();
        let save = |sql: &str| {
            let mut task = new_task(sql);
            let ids_seen: Vec<String> = task.steps.iter().map(|s| s.id.clone()).collect();
            task.id = "t1".into();
            task.steps[0].id = "s1".into();
            approve_writes(&state, &mut task, &ids_seen, Some(&checked)).map(|_| task)
        };
        let saved = save("DELETE FROM t").unwrap();
        // Stored as the run computes it (with the step's id).
        assert_eq!(saved.approved_writes, tasks::write_fingerprint(&state, &saved));
        // Changed between the check and the save: refused, asked again.
        assert!(save("DROP TABLE t").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

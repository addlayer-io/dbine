//! Scheduled tasks (docs/scheduled-tasks.md): running one, in the app
//! ("Ejecutar ahora") or by itself (`dbine --run-task <id>`, started by the
//! OS scheduler), and keeping the OS scheduler in step with the list.
//!
//! The model lives in `dbine_core::tasks`; each step kind is a function in
//! `steps.rs` that takes its JSON config and returns what it did, so a new
//! kind is one more arm there.

pub mod headless;
mod i18n;
pub mod mail;
pub mod message;
pub mod notify;
pub mod os;
mod steps;

/// What a scheduled notebook export says where a chart would be.
#[cfg(test)]
pub(crate) use steps::CHART_NOTE;

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::tasks::{self, kinds, Notify, OnError, RunStatus, ScheduledTask, Step, StepRun, TaskRun};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tauri::AppHandle;

/// Rows kept per result set of a script step (only counts are recorded).
const SCRIPT_MAX_ROWS: usize = 100;

/// A step's settings that point at a database.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Target {
    #[serde(default)]
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
}

/// A step that changes data or structure, as the approval shows it.
#[derive(Debug, Clone, Serialize)]
pub struct WriteScope {
    pub step_id: String,
    pub connection_id: String,
    pub connection: String,
    pub database: String,
    /// The first statement that writes (`DELETE`, `ALTER`…), or why it
    /// counts as one ("script de MongoDB").
    pub what: String,
    /// The connection carries a production tag (or, for a notebook's
    /// cell, its environment asks before running).
    pub production: bool,
    /// A notebook step's cell that writes (its name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    /// The script that writes (the fingerprint covers it).
    #[serde(skip)]
    pub script: String,
}

/// Tags that mark a production connection.
pub(crate) const PROD_TAGS: &[&str] = &["prod", "production", "produccion", "producción", "prd"];

fn target(step: &Step) -> Target {
    serde_json::from_value(step.config.clone()).unwrap_or_default()
}

fn text<'a>(config: &'a Value, key: &str) -> &'a str {
    config.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The steps of `task` that change data or structure. Only scripts and
/// notebooks' cells can: an export reads, a comparison writes its script to
/// a file and a backup doesn't change the database.
pub fn write_scope(state: &AppState, task: &ScheduledTask) -> Vec<WriteScope> {
    // With the vault's parameter values: they are what the cells run with.
    write_scope_in(state, &with_secrets(task), &load_notebooks(task))
}

/// The notebook of each step that runs one (by position; `None` for the
/// other steps), read once: [`run_task`] checks the approval on these
/// copies and the steps run them.
pub(crate) type Notebooks = Vec<Option<CommandResult<crate::notebook::Notebook>>>;

pub(crate) fn load_notebooks(task: &ScheduledTask) -> Notebooks {
    task.steps
        .iter()
        .map(|step| {
            (step.kind == kinds::RUN_NOTEBOOK).then(|| {
                serde_json::from_value::<crate::notebook::headless::Options>(step.config.clone())
                    .map_err(|e| CommandError::BadRequest(format!("la configuración del paso no es válida: {e}")))
                    .and_then(|o| crate::notebook::headless::load(&o))
            })
        })
        .collect()
}

fn write_scope_in(state: &AppState, task: &ScheduledTask, notebooks: &Notebooks) -> Vec<WriteScope> {
    let mut out = Vec::new();
    for (n, step) in task.steps.iter().enumerate() {
        match step.kind.as_str() {
            kinds::RUN_SCRIPT => script_scope(state, step, &mut out),
            kinds::RUN_NOTEBOOK => {
                // A notebook that can't be read runs nothing: its step fails.
                if let Some(Some(Ok(nb))) = notebooks.get(n) {
                    notebook_scope(state, step, nb, &mut out);
                }
            }
            _ => {}
        }
    }
    out
}

/// A notebook step's cells that write, in `nb` (the notebook as read now
/// from disk), so editing those cells asks for the approval again.
fn notebook_scope(state: &AppState, step: &Step, nb: &crate::notebook::Notebook, out: &mut Vec<WriteScope>) {
    let Ok(opts) = serde_json::from_value::<crate::notebook::headless::Options>(step.config.clone()) else { return };
    for (cell, r, what, sql) in crate::notebook::headless::writing_cells(state, &opts, nb) {
        let connection = state.store.get_connection(&r.connection_id).ok().flatten().map(|c| c.name).unwrap_or_default();
        out.push(WriteScope {
            step_id: step.id.clone(),
            connection_id: r.connection_id.clone(),
            connection,
            database: r.database.clone(),
            what,
            production: r.needs_opt_in(),
            cell: Some(cell),
            script: format!("{}\u{0}{sql}", opts.path.trim()),
        });
    }
}

fn script_scope(state: &AppState, step: &Step, out: &mut Vec<WriteScope>) {
    let t = target(step);
    let Ok(Some(conn)) = state.store.get_connection(&t.connection_id) else { return };
    // A read-only connection stays read-only: nothing to approve.
    if conn.config.read_only {
        return;
    }
    let sql = text(&step.config, "sql");
    let what = match dbine_drivers::find(&conn.config.driver) {
        Some(d) if d.info().language == dbine_driver::Language::Sql => {
            dbine_driver::read_only::first_write_in(sql, &d.script_dialect())
        }
        // Not SQL: no reliable way to tell a read from a write.
        Some(d) => (!sql.trim().is_empty()).then(|| format!("script de {}", d.info().name)),
        None => None,
    };
    let Some(what) = what else { return };
    let production = conn.tags.iter().any(|t| PROD_TAGS.contains(&t.trim().to_lowercase().as_str()));
    out.push(WriteScope {
        step_id: step.id.clone(),
        connection_id: t.connection_id,
        connection: conn.name,
        database: t.database,
        what,
        production,
        cell: None,
        script: sql.to_string(),
    });
}

/// The fingerprint of what writes: connection, database and script of each
/// writing step, and where its messages go. `None` when nothing writes.
pub fn write_fingerprint(state: &AppState, task: &ScheduledTask) -> Option<String> {
    task_fingerprint(&write_scope(state, task), task)
}

/// The fingerprint of `scope` alone (a task without message steps).
#[cfg(test)]
pub fn fingerprint_of(scope: &[WriteScope]) -> Option<String> {
    fingerprint_with(scope, &[])
}

/// [`fingerprint_of`] `scope` plus where `task`'s "Enviar un mensaje" steps
/// send (channel and host, never the secret part: [`message::destinations`]),
/// so a task that writes asks for the approval again when a message's
/// destination changes. Read only when something writes.
pub fn task_fingerprint(scope: &[WriteScope], task: &ScheduledTask) -> Option<String> {
    if scope.is_empty() {
        return None;
    }
    fingerprint_with(scope, &message::destinations(task, &|name| dbine_core::secrets::get_raw(name).ok().flatten()))
}

/// Without destinations it is what it was before messages existed, so the
/// approvals already given stand.
pub(crate) fn fingerprint_with(scope: &[WriteScope], sends_to: &[String]) -> Option<String> {
    if scope.is_empty() {
        return None;
    }
    let mut h = Sha256::new();
    for w in scope {
        for part in [w.step_id.as_str(), &w.connection_id, &w.database, &w.script] {
            h.update(part.as_bytes());
            h.update([0u8]);
        }
    }
    for d in sends_to {
        h.update(b"\x01send\x00");
        h.update(d.as_bytes());
        h.update([0u8]);
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Vault name of a step's secret options (a backup's encryption password):
/// they never go in the task's JSON.
pub fn step_secret_name(task_id: &str, step_id: &str) -> String {
    format!("task-secret:{task_id}:{step_id}")
}

/// A value that counts as not given (the vault's, if any, stands).
fn blank(v: &Value) -> bool {
    v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty())
}

/// What the vault keeps for a "Ejecutar notebook" step: its credential
/// parameters' values (a password's, a token's), by name.
type NotebookSecrets = BTreeMap<String, Value>;

/// Takes the credential parameters' values out of each "Ejecutar notebook"
/// step: the task's JSON keeps none (the vault does). Returns, per step,
/// its vault entry and the values to keep there (`None`: delete it). An
/// empty value with one already kept means "unchanged", as a backup's
/// secret options. `get`: a vault entry by name.
pub(crate) fn notebook_secrets_apart(task: &mut ScheduledTask, get: &dyn Fn(&str) -> Option<String>) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for step in task.steps.iter_mut().filter(|s| s.kind == kinds::RUN_NOTEBOOK) {
        let name = step_secret_name(&task.id, &step.id);
        let mut kept: NotebookSecrets = get(&name).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        if let Some(params) = step.config.get_mut("params").and_then(Value::as_object_mut) {
            let secret: Vec<String> = params.keys().filter(|k| crate::notebook::is_credential_param(k)).cloned().collect();
            for k in secret {
                if let Some(v) = params.remove(&k).filter(|v| !blank(v)) {
                    kept.insert(k, v);
                }
            }
        }
        out.push((name, (!kept.is_empty()).then(|| serde_json::to_string(&kept).unwrap_or_default())));
    }
    out
}

/// `task` with its "Ejecutar notebook" steps' credential parameter values
/// back from the vault (where the step has none of its own): what the
/// approval's fingerprint and the run use.
pub fn with_secrets(task: &ScheduledTask) -> ScheduledTask {
    with_secrets_from(task, &|name| dbine_core::secrets::get_raw(name).ok().flatten())
}

pub(crate) fn with_secrets_from(task: &ScheduledTask, get: &dyn Fn(&str) -> Option<String>) -> ScheduledTask {
    let mut task = task.clone();
    // A task not saved yet has nothing in the vault.
    if task.id.is_empty() {
        return task;
    }
    for step in task.steps.iter_mut().filter(|s| s.kind == kinds::RUN_NOTEBOOK && !s.id.is_empty()) {
        let Some(kept) = get(&step_secret_name(&task.id, &step.id)).and_then(|j| serde_json::from_str::<NotebookSecrets>(&j).ok()) else { continue };
        let Some(config) = step.config.as_object_mut() else { continue };
        let params = config.entry("params").or_insert_with(|| Value::Object(Default::default()));
        let Some(params) = params.as_object_mut() else { continue };
        for (k, v) in kept {
            if params.get(&k).is_none_or(blank) {
                params.insert(k, v);
            }
        }
    }
    task
}

/// The keys of a backup step's options that are secret.
pub fn secret_option_keys(connection_id: &str, state: &AppState) -> Vec<&'static str> {
    let Ok(driver) = crate::commands::schema::driver_of(state, connection_id) else { return Vec::new() };
    driver
        .backup()
        .map(|s| s.backup_options.iter().filter(|f| f.secret || matches!(f.kind, dbine_driver::FieldKind::Password)).map(|f| f.key).collect())
        .unwrap_or_default()
}

/// The connections a task's steps use (to read their secrets up front).
pub fn connections_of(state: &AppState, task: &ScheduledTask) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for step in &task.steps {
        let ids = match step.kind.as_str() {
            kinds::RUN_NOTEBOOK => serde_json::from_value::<crate::notebook::headless::Options>(step.config.clone())
                .map(|o| crate::notebook::headless::connections(state, &o))
                .unwrap_or_default(),
            kinds::COMPARE_SCHEMAS => ["source", "target"]
                .iter()
                .filter_map(|k| step.config.get(*k).and_then(|t| t.get("connection_id")).and_then(Value::as_str).map(str::to_string))
                .collect(),
            _ => vec![target(step).connection_id],
        };
        for id in ids.into_iter().filter(|i| !i.is_empty()) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// What a step did, before the runner stamps it.
#[derive(Default)]
pub(crate) struct StepDone {
    pub summary: String,
    pub messages: Vec<String>,
    pub outputs: BTreeMap<String, String>,
    pub alert: Option<String>,
}

pub(crate) struct Ctx<'a> {
    pub state: &'a AppState,
    pub app: Option<&'a AppHandle>,
    pub task: &'a ScheduledTask,
    /// Prefix of the session keys of this run (`cancel_query` takes them).
    pub run_key: String,
}

/// A request to cancel a run, as a machine-local setting: the run may be in
/// another process (the OS scheduler's `dbine --run-task`), which watches it.
pub fn cancel_key(run_id: &str) -> String {
    format!("local.task-cancel.{run_id}")
}

/// Run a task now. The run is saved as it goes; the result is the finished
/// run (also when it failed: its `status` says so).
pub async fn run_task(state: &AppState, app: Option<&AppHandle>, task: &ScheduledTask, trigger: &str) -> TaskRun {
    let started = chrono::Local::now().naive_local();
    let mut run = TaskRun {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task.id.clone(),
        trigger: trigger.into(),
        status: RunStatus::Running,
        started_at: tasks::now_text(),
        ..Default::default()
    };
    save(state, &run);
    tracing::info!(task = %task.id, run = %run.id, trigger, "scheduled task started");
    // The notebook steps' credential parameters, from the vault.
    let task = &with_secrets(task);

    // Steps that write run only as approved. Each notebook is read once,
    // here: the steps run these same copies, so a file changed (or
    // unreadable) at this point can't put back other cells after the check.
    let mut notebooks = load_notebooks(task);
    let fingerprint = task_fingerprint(&write_scope_in(state, task, &notebooks), task);
    if fingerprint.is_some() && fingerprint != task.approved_writes {
        run.error = Some("La tarea cambia datos o estructura y esos pasos cambiaron desde que se aprobaron: abrila en DBine y volvé a guardarla para aprobarlos.".into());
        return finish(state, run, task);
    }

    let mut vars = tasks::base_vars(task, started);
    let ctx = Ctx { state, app, task, run_key: format!("task:{}", run.id) };
    // Cancelled from the phone app (or anywhere that sets the request): cut
    // the steps' sessions at once and run no further step.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let (state, cancelled, key, prefix) = (state.clone(), cancelled.clone(), cancel_key(&run.id), format!("{}:", ctx.run_key));
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if state.store.get_setting(&key).ok().flatten().is_some() {
                    cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                    // A notebook step's DuckDB session is `nbx:<prefix>…`.
                    let duck = format!("nbx:{prefix}");
                    let keys: Vec<String> = state.sessions.iter().map(|e| e.key().clone()).filter(|k| k.starts_with(&prefix) || k.starts_with(&duck)).collect();
                    for k in keys {
                        crate::commands::query::cancel_session(&state, &k);
                    }
                }
            }
        })
    };
    let mut any_failed = false;
    // `{results}` and `{alerts}` leave DBine (a message, a mail): in the UI language.
    let lang = i18n::ui_language(state);
    for (n, step) in task.steps.iter().enumerate() {
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        let step_started = tasks::now_text();
        let label = if step.name.is_empty() { step.kind.clone() } else { step.name.clone() };
        // What the earlier steps did and raised (a message's or a mail's text).
        vars.insert("results".into(), results_text(task, &run.steps, &lang));
        vars.insert("alerts".into(), run.steps.iter().filter_map(|s| s.alert.as_deref()).map(|a| i18n::translate(&lang, a)).collect::<Vec<_>>().join("\n"));
        let result = match skipped(step, &run.steps) {
            Some(why) => Ok(StepDone { summary: why.into(), ..Default::default() }),
            None => steps::run(&ctx, step, &vars, notebooks.get_mut(n).and_then(Option::take)).await,
        };
        let mut sr = StepRun { step_id: step.id.clone(), kind: step.kind.clone(), started_at: step_started, finished_at: tasks::now_text(), ..Default::default() };
        let stop = match result {
            Ok(done) => {
                sr.status = RunStatus::Ok;
                sr.summary = done.summary;
                sr.messages = done.messages;
                sr.alert = done.alert;
                for (k, v) in &done.outputs {
                    vars.insert(format!("steps.{}.{k}", n + 1), v.clone());
                }
                sr.outputs = done.outputs;
                false
            }
            Err(e) => {
                tracing::warn!(task = %task.id, step = %label, "scheduled task step failed: {e}");
                sr.status = RunStatus::Failed;
                sr.summary = e.to_string();
                any_failed = true;
                step.on_error == OnError::Stop
            }
        };
        run.steps.push(sr);
        save(state, &run);
        if stop {
            break;
        }
    }
    watcher.abort();
    let _ = state.store.set_setting(&cancel_key(&run.id), None::<&serde_json::Value>);
    if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        run.error = Some("Se canceló la tarea.".into());
        return finish(state, run, task);
    }
    run.status = match (any_failed, run.steps.iter().any(|s| s.status == RunStatus::Ok)) {
        (false, _) => RunStatus::Ok,
        (true, true) if run.steps.len() == task.steps.len() => RunStatus::Partial,
        (true, _) => RunStatus::Failed,
    };
    finish(state, run, task)
}

const SKIPPED_NO_ALERT: &str = "No se ejecutó: ningún paso anterior tuvo alertas.";
const SKIPPED_NO_ERROR: &str = "No se ejecutó: ningún paso anterior falló.";

/// Why a step doesn't run, from its "Solo si…" (`config.when`, any kind):
/// `always` (or none), `alert` (an earlier step had an alert) or `errors`
/// (an earlier step failed and the task went on).
fn skipped(step: &Step, before: &[StepRun]) -> Option<&'static str> {
    match text(&step.config, "when") {
        "alert" if !before.iter().any(|s| s.alert.is_some()) => Some(SKIPPED_NO_ALERT),
        "errors" if !before.iter().any(|s| s.status == RunStatus::Failed) => Some(SKIPPED_NO_ERROR),
        _ => None,
    }
}

/// The variables that carry text from the databases (`{results}` has an
/// earlier step's error as the server wrote it; `{alerts}` too). Only a
/// message's or a mail's text may use them: in SQL, a path, an address or an
/// option, crafted data echoed in an error could inject SQL or pick a file.
const DB_TEXT_VARS: [&str; 2] = ["results", "alerts"];

/// The task's variables without [`DB_TEXT_VARS`], for everything that isn't
/// a message's or a mail's text: `{results}` and `{alerts}` stay as written.
pub(crate) fn without_db_text(vars: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    vars.iter().filter(|(k, _)| !DB_TEXT_VARS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// `{results}`: one line per step that ran before, with how it went (✓
/// fine, ✗ failed, – skipped by its "Solo si…"), its number, name and
/// summary, the summary in `lang` (the UI language).
fn results_text(task: &ScheduledTask, done: &[StepRun], lang: &str) -> String {
    done.iter()
        .enumerate()
        .map(|(n, r)| {
            let name = task.steps.iter().find(|s| s.id == r.step_id).map(|s| if s.name.trim().is_empty() { s.kind.clone() } else { s.name.trim().to_string() }).unwrap_or_default();
            let mark = match r.status {
                RunStatus::Failed => "✗",
                _ if [SKIPPED_NO_ALERT, SKIPPED_NO_ERROR].contains(&r.summary.as_str()) => "–",
                _ => "✓",
            };
            format!("{mark} {}. {name}: {}", n + 1, i18n::translate(lang, &r.summary))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn save(state: &AppState, run: &TaskRun) {
    if let Err(e) = state.store.save_task_run(run) {
        tracing::warn!("could not record the task run: {e}");
    }
}

fn finish(state: &AppState, mut run: TaskRun, task: &ScheduledTask) -> TaskRun {
    if run.error.is_some() {
        run.status = RunStatus::Failed;
    }
    run.finished_at = tasks::now_text();
    save(state, &run);
    tracing::info!(task = %task.id, run = %run.id, status = ?run.status, "scheduled task finished");
    let alerts: Vec<&str> = run.steps.iter().filter_map(|s| s.alert.as_deref()).collect();
    let tell = match task.notify {
        Notify::Never => false,
        Notify::Always => true,
        Notify::Failure => run.status != RunStatus::Ok || !alerts.is_empty(),
    };
    if tell {
        let body = match run.status {
            RunStatus::Ok if alerts.is_empty() => "Terminó bien.".to_string(),
            RunStatus::Ok => alerts.join(" "),
            RunStatus::Partial => format!("Terminó con errores. {}", alerts.join(" ")).trim().to_string(),
            _ => run
                .error
                .clone()
                .or_else(|| run.steps.iter().find(|s| s.status == RunStatus::Failed).map(|s| format!("Falló: {}", s.summary)))
                .unwrap_or_else(|| "Falló.".into()),
        };
        notify::send(&format!("DBine · {}", task.name), &body);
    }
    run
}

/// The task, or a NotFound the caller can show.
pub fn load(state: &AppState, id: &str) -> CommandResult<ScheduledTask> {
    state.store.get_task(id)?.ok_or_else(|| CommandError::NotFound(format!("no existe la tarea programada '{id}'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_core::state::SavedConnection;
    use dbine_core::StateStore;
    use dbine_driver::ConnectionConfig;
    use serde_json::json;

    struct World {
        state: AppState,
        dir: std::path::PathBuf,
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Two SQLite files ("a" with a table more than "b") as saved connections.
    fn world(name: &str) -> World {
        let dir = std::env::temp_dir().join(format!("dbine-tasks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState::new(StateStore::open_in_memory().unwrap());
        for (id, sql, tags) in [
            ("a", "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT); CREATE TABLE extra (x INTEGER); INSERT INTO people (name) VALUES ('ana'), ('beto');", vec!["prod".to_string()]),
            ("b", "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL);", vec![]),
        ] {
            let file = dir.join(format!("{id}.sqlite"));
            rusqlite::Connection::open(&file).unwrap().execute_batch(sql).unwrap();
            state
                .store
                .save_connection(&SavedConnection {
                    id: id.into(),
                    name: format!("base {id}"),
                    color: None,
                    config: ConnectionConfig { driver: "sqlite".into(), host: file.to_string_lossy().into_owned(), ..Default::default() },
                    save_password: false,
                    folder_id: None,
                    tags,
                    mcp_level: None,
                    updated_at: String::new(),
                })
                .unwrap();
        }
        World { state, dir }
    }

    fn step(id: &str, kind: &str, config: Value) -> Step {
        Step { id: id.into(), kind: kind.into(), config, ..Default::default() }
    }

    fn task(steps: Vec<Step>) -> ScheduledTask {
        ScheduledTask { id: "t".into(), name: "prueba".into(), enabled: true, steps, notify: Notify::Never, ..Default::default() }
    }

    /// A notebook step's credential parameters never stay in the task's
    /// JSON: the vault keeps them and the run gets them back.
    #[test]
    fn notebook_credential_parameters_go_to_the_vault() {
        use std::cell::RefCell;
        use std::collections::HashMap;
        let vault: RefCell<HashMap<String, String>> = RefCell::default();
        let get = |n: &str| vault.borrow().get(n).cloned();
        let save = |t: &mut ScheduledTask| {
            for (name, kept) in notebook_secrets_apart(t, &get) {
                match kept {
                    Some(json) => vault.borrow_mut().insert(name, json),
                    None => vault.borrow_mut().remove(&name),
                };
            }
        };
        let mut t = task(vec![step("nb", kinds::RUN_NOTEBOOK, json!({"path": "/x.ipynb", "params": {"db_password": "hunter2-secret", "api_token": 42, "desde": "2026-01-01"}}))]);
        save(&mut t);
        let stored = serde_json::to_string(&t).unwrap();
        assert!(!stored.contains("hunter2") && !stored.contains("42"), "{stored}");
        assert_eq!(t.steps[0].config["params"], json!({"desde": "2026-01-01"}));
        assert!(vault.borrow()[&step_secret_name("t", "nb")].contains("hunter2-secret"));

        // The run (and the approval's fingerprint) see the values again.
        let run = with_secrets_from(&t, &get);
        let opts: crate::notebook::headless::Options = serde_json::from_value(run.steps[0].config.clone()).unwrap();
        assert_eq!(opts.params.get("db_password"), Some(&json!("hunter2-secret")));
        assert_eq!(opts.params.get("api_token"), Some(&json!(42)));
        assert_eq!(opts.params.get("desde"), Some(&json!("2026-01-01")));

        // Saved again with the field left empty: the kept value stands; a
        // new one replaces it.
        let mut again = t.clone();
        again.steps[0].config["params"]["db_password"] = json!("");
        save(&mut again);
        assert_eq!(with_secrets_from(&again, &get).steps[0].config["params"]["db_password"], json!("hunter2-secret"));
        again.steps[0].config["params"]["db_password"] = json!("new-pass");
        save(&mut again);
        assert!(!serde_json::to_string(&again).unwrap().contains("new-pass"));
        assert_eq!(with_secrets_from(&again, &get).steps[0].config["params"]["db_password"], json!("new-pass"));
        // Other steps are left alone.
        let mut other = task(vec![step("s", kinds::RUN_SCRIPT, json!({"connection_id": "a", "sql": "SELECT 1", "params": {"pwd": "x"}}))]);
        assert!(notebook_secrets_apart(&mut other, &get).is_empty());
        assert_eq!(other.steps[0].config["params"]["pwd"], json!("x"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_cancelled_from_elsewhere_stops() {
        let w = world("cancel");
        let long = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c LIMIT 400000000) SELECT count(*) FROM c";
        let t = task(vec![
            step("s1", kinds::RUN_SCRIPT, json!({"connection_id": "b", "sql": long})),
            step("s2", kinds::RUN_SCRIPT, json!({"connection_id": "b", "sql": "SELECT 1"})),
        ]);
        let state = w.state.clone();
        let started = std::time::Instant::now();
        let run = tokio::spawn({
            let t = t.clone();
            async move { run_task(&state, None, &t, "manual").await }
        });
        // The run shows as running: ask to cancel it, as the phone app does.
        let id = loop {
            let runs = w.state.store.list_task_runs(Some("t"), 1).unwrap();
            if let Some(r) = runs.into_iter().find(|r| r.status == RunStatus::Running) {
                break r.id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        w.state.store.set_setting(&cancel_key(&id), Some(&json!(true))).unwrap();
        let run = run.await.unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.error.as_deref(), Some("Se canceló la tarea."));
        assert!(run.steps.len() <= 1, "the next step doesn't run: {:?}", run.steps);
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "cut, not waited out: {:?}", started.elapsed());
        assert!(w.state.store.get_setting(&cancel_key(&id)).unwrap().is_none(), "the request is cleared");
    }

    #[tokio::test]
    async fn writes_need_approval() {
        let w = world("approval");
        let mut t = task(vec![
            step("s1", kinds::RUN_SCRIPT, json!({"connection_id": "a", "database": "main", "sql": "SELECT 1"})),
            step("s2", kinds::RUN_SCRIPT, json!({"connection_id": "a", "database": "main", "sql": "SELECT 1;\nDELETE FROM people"})),
        ]);
        let scope = write_scope(&w.state, &t);
        assert_eq!(scope.len(), 1);
        assert_eq!((scope[0].step_id.as_str(), scope[0].what.as_str(), scope[0].production), ("s2", "DELETE", true));

        // Not approved: nothing runs.
        let run = run_task(&w.state, None, &t, "manual").await;
        assert_eq!(run.status, RunStatus::Failed);
        assert!(run.steps.is_empty() && run.error.is_some());
        let rows: i64 = rusqlite::Connection::open(w.dir.join("a.sqlite")).unwrap().query_row("SELECT COUNT(*) FROM people", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2);

        // Approved: it runs; edited afterwards: it asks again.
        t.approved_writes = write_fingerprint(&w.state, &t);
        let run = run_task(&w.state, None, &t, "manual").await;
        assert_eq!(run.status, RunStatus::Ok, "{run:?}");
        assert_eq!(run.steps[1].outputs["affected"], "2");
        t.steps[1].config["sql"] = json!("DELETE FROM extra");
        assert_ne!(write_fingerprint(&w.state, &t), t.approved_writes);
        assert_eq!(w.state.store.list_task_runs(Some("t"), 10).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn export_compare_and_copy() {
        let w = world("steps");
        let out = w.dir.join("out");
        let folder = out.to_string_lossy().into_owned();
        let t = task(vec![
            step("e", kinds::EXPORT, json!({"connection_id": "a", "database": "main", "sql": "SELECT name FROM people ORDER BY id", "folder": folder, "file_name": "{task}-gente", "options": {"format": "csv", "header": true}})),
            step("c", kinds::COMPARE_SCHEMAS, json!({"source": {"connection_id": "a", "database": "main"}, "target": {"connection_id": "b", "database": "main"}, "folder": folder, "file_name": "sync"})),
            step("b", kinds::BACKUP, json!({"connection_id": "b", "database": "main", "mode": "copy", "folder": folder, "file_name": "copia-{steps.1.rows}"})),
        ]);
        let run = run_task(&w.state, None, &t, "manual").await;
        assert_eq!(run.status, RunStatus::Ok, "{run:#?}");

        let csv = std::fs::read_to_string(out.join("prueba-gente.csv")).unwrap();
        assert_eq!(csv.lines().collect::<Vec<_>>(), ["name", "ana", "beto"]);

        let compare = &run.steps[1];
        assert_eq!(compare.outputs["differences"], "2");
        assert!(compare.alert.is_some());
        let script = std::fs::read_to_string(out.join("sync.sql")).unwrap();
        assert!(script.contains("extra") && script.to_lowercase().contains("email"), "{script}");

        // The export's row count went into the copy's name.
        assert!(out.join("copia-2.sql").exists());
        assert_eq!(run.steps[2].outputs["file"], out.join("copia-2.sql").to_string_lossy());
    }

    #[tokio::test]
    async fn failing_step_stops_or_continues() {
        let w = world("onerror");
        let bad = |on_error| Step { on_error, ..step("x", kinds::RUN_SCRIPT, json!({"connection_id": "b", "database": "main", "sql": "SELECT * FROM nowhere"})) };
        let good = step("y", kinds::RUN_SCRIPT, json!({"connection_id": "b", "database": "main", "sql": "SELECT 1"}));

        let run = run_task(&w.state, None, &task(vec![bad(OnError::Stop), good.clone()]), "manual").await;
        assert_eq!((run.status, run.steps.len()), (RunStatus::Failed, 1));
        assert!(run.steps[0].summary.contains("nowhere"), "{}", run.steps[0].summary);

        let run = run_task(&w.state, None, &task(vec![bad(OnError::Continue), good]), "manual").await;
        assert_eq!((run.status, run.steps.len()), (RunStatus::Partial, 2));
    }

    /// A message step's "Solo si…" works as the mail's, and `{results}`
    /// says what each earlier step did. (No step id: the run reads no vault,
    /// and no URL: nothing leaves the machine.)
    #[tokio::test]
    async fn message_step_only_if_and_results() {
        let w = world("message-when");
        let ok = step("ok", kinds::RUN_SCRIPT, json!({"connection_id": "b", "database": "main", "sql": "SELECT 1"}));
        let bad = Step { on_error: OnError::Continue, name: "falla".into(), ..step("bad", kinds::RUN_SCRIPT, json!({"connection_id": "b", "database": "main", "sql": "SELECT * FROM nowhere"})) };
        let message = |when: &str| step("", kinds::SEND_MESSAGE, json!({"channel": "slack", "url": "", "text": "{results}", "when": when}));

        for when in ["alert", "errors"] {
            let run = run_task(&w.state, None, &task(vec![ok.clone(), message(when)]), "manual").await;
            assert_eq!(run.status, RunStatus::Ok, "{when}");
            assert!(run.steps[1].summary.starts_with("No se ejecutó"), "{when}: {}", run.steps[1].summary);
        }
        // It runs: always, or after a failure (and fails here, with no webhook).
        for (before, when) in [(ok.clone(), "always"), (bad.clone(), "errors")] {
            let run = run_task(&w.state, None, &task(vec![before, message(when)]), "manual").await;
            assert_eq!(run.steps[1].status, RunStatus::Failed, "{when}");
            assert!(run.steps[1].summary.contains("falta la URL del webhook de Slack"), "{}", run.steps[1].summary);
        }

        let t = task(vec![ok, bad, message("alert")]);
        let run = run_task(&w.state, None, &t, "manual").await;
        let results = results_text(&t, &run.steps[..2], "es");
        let lines: Vec<&str> = results.lines().collect();
        assert!(lines[0].starts_with("✓ 1. run_script: "), "{results}");
        assert!(lines[1].starts_with("✗ 2. falla: ") && lines[1].contains("nowhere"), "{results}");
        assert!(results_text(&t, &run.steps, "es").lines().nth(2).unwrap().starts_with("– 3. send_message: No se ejecutó"));
        // Text from the databases never reaches SQL, a path or an address.
        let mut vars = BTreeMap::new();
        vars.insert("results".to_string(), "x'); DROP TABLE t; --".to_string());
        vars.insert("alerts".to_string(), "../../etc".to_string());
        vars.insert("task".to_string(), "nightly".to_string());
        let plain = without_db_text(&vars);
        assert_eq!(tasks::expand("INSERT INTO log VALUES ('{results}', '{alerts}', '{task}')", &plain), "INSERT INTO log VALUES ('{results}', '{alerts}', 'nightly')");
        // In another UI language the summaries are translated, the marks stay.
        assert!(results_text(&t, &run.steps, "en").lines().nth(2).unwrap().starts_with("– 3. send_message: Not run: "));
    }

    /// A task that writes asks for its approval again when a message's
    /// destination host changes; the secret part of the URL never counts.
    /// One that doesn't write has no approval at all.
    #[test]
    fn message_destination_is_part_of_the_approval() {
        let w = world("message-fp");
        let write = step("s", kinds::RUN_SCRIPT, json!({"connection_id": "a", "database": "main", "sql": "DELETE FROM people"}));
        let message = |url: &str| step("m", kinds::SEND_MESSAGE, json!({"channel": "slack", "url": url}));
        let none = |_: &str| None;
        let fp = |t: &ScheduledTask| fingerprint_with(&write_scope(&w.state, t), &message::destinations(t, &none));
        let a = fp(&task(vec![write.clone(), message("https://hooks.slack.com/services/T/B/one")]));
        let same_host = fp(&task(vec![write.clone(), message("https://hooks.slack.com/services/T/B/two")]));
        let teams = fp(&task(vec![write.clone(), step("m", kinds::SEND_MESSAGE, json!({"channel": "teams", "url": "https://x.webhook.office.com/a"}))]));
        assert!(a.is_some());
        assert_eq!(a, same_host);
        assert_ne!(a, teams);
        // Without messages, the fingerprint is the one approvals already have.
        assert_eq!(fp(&task(vec![write.clone()])), fingerprint_of(&write_scope(&w.state, &task(vec![write]))));
        assert_eq!(fp(&task(vec![message("https://hooks.slack.com/services/T/B/one")])), None);
    }

    #[tokio::test]
    async fn only_if_conditions() {
        let w = world("when");
        let ok = step("ok", kinds::RUN_SCRIPT, json!({"connection_id": "b", "database": "main", "sql": "SELECT 1"}));
        let bad = Step { on_error: OnError::Continue, ..step("bad", kinds::RUN_SCRIPT, json!({"connection_id": "b", "database": "main", "sql": "SELECT * FROM nowhere"})) };
        let mail = |when: &str| step("m", kinds::SEND_MAIL, json!({"to": "a@example.com", "when": when}));

        // Nothing to tell: the mail doesn't go (and the step is fine).
        let run = run_task(&w.state, None, &task(vec![ok.clone(), mail("alert")]), "manual").await;
        assert_eq!(run.status, RunStatus::Ok);
        assert!(run.steps[1].summary.starts_with("No se ejecutó"), "{}", run.steps[1].summary);
        let run = run_task(&w.state, None, &task(vec![ok, mail("errors")]), "manual").await;
        assert!(run.steps[1].summary.starts_with("No se ejecutó"));

        // An earlier failure: it runs (and fails here, with no server set).
        let run = run_task(&w.state, None, &task(vec![bad, mail("errors")]), "manual").await;
        assert_eq!(run.steps[1].status, RunStatus::Failed);
        assert!(run.steps[1].summary.contains("Configuración › Correo"), "{}", run.steps[1].summary);
    }

    #[test]
    fn secrets_stay_out_of_the_config() {
        let t = task(vec![step("s", kinds::BACKUP, json!({"connection_id": "a", "options": {"password": "x"}}))]);
        assert_eq!(connections_of(&AppState::new(StateStore::open_in_memory().unwrap()), &t), ["a"]);
        assert_eq!(step_secret_name("t", "s"), "task-secret:t:s");
    }
}

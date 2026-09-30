//! "Clonar tabla" (explorer's context menu): a copy of a table next to it,
//! under a new name, with its data. The work is `dbine_transfer::clone_table`;
//! this runs it over dedicated connections (the source side read-only) and
//! reports progress as `clone-table-progress` events, tagged with the run id.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::{async_trait, Driver, ObjectRef, Session};
use dbine_transfer::clone_table::{clone_table as run_clone, CloneControl, CloneEvent, CloneOptions, CloneRequest, Rename};
use dbine_transfer::Endpoints;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use tauri::{AppHandle, Emitter, State};

/// Running clones, by run id (for `clone_table_cancel`).
static RUNNING: LazyLock<Mutex<HashMap<String, CloneControl>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn running() -> std::sync::MutexGuard<'static, HashMap<String, CloneControl>> {
    RUNNING.lock().unwrap_or_else(|p| p.into_inner())
}

/// The clone's database, one new connection per use (the source read-only).
struct AppEndpoints {
    state: AppState,
    run_id: String,
    connection_id: String,
    database: String,
    driver: Arc<dyn Driver>,
    seq: AtomicU64,
}

impl AppEndpoints {
    async fn open(&self, read_only: bool) -> dbine_driver::Result<Box<dyn Session>> {
        let key = format!("clone:{}:{}", self.run_id, self.seq.fetch_add(1, Ordering::Relaxed));
        let entry = self.state.dedicated_session(&key, &self.connection_id, &self.database, read_only).await.map_err(driver_error);
        // The clone owns the connection: it leaves the app's map.
        self.state.sessions.remove(&key);
        let entry = Arc::try_unwrap(entry?).map_err(|_| dbine_driver::Error::State("la conexión quedó compartida".into()))?;
        Ok(entry.session.into_inner())
    }
}

fn driver_error(e: CommandError) -> dbine_driver::Error {
    use dbine_driver::Error;
    match e {
        CommandError::Connect(m) => Error::Connect(m),
        CommandError::AuthFailed(m) => Error::AuthFailed(m),
        CommandError::PasswordRequired(_) => Error::AuthFailed(e.to_string()),
        CommandError::Cancelled => Error::Cancelled,
        other => Error::State(other.to_string()),
    }
}

#[async_trait]
impl Endpoints for AppEndpoints {
    fn source_driver(&self) -> Arc<dyn Driver> {
        self.driver.clone()
    }
    fn target_driver(&self) -> Arc<dyn Driver> {
        self.driver.clone()
    }
    async fn open_source(&self) -> dbine_driver::Result<Box<dyn Session>> {
        self.open(true).await
    }
    async fn open_target(&self) -> dbine_driver::Result<Box<dyn Session>> {
        self.open(false).await
    }
}

#[derive(Deserialize)]
pub struct CloneArgs {
    /// Chosen by the UI: its events carry it and `clone_table_cancel` takes it.
    pub run_id: String,
    pub connection_id: String,
    pub database: String,
    pub object: ObjectRef,
    pub new_name: String,
    #[serde(default = "yes")]
    pub with_data: bool,
    #[serde(default = "yes")]
    pub with_indexes: bool,
}

fn yes() -> bool {
    true
}

#[derive(Serialize)]
pub struct CloneResult {
    pub table: ObjectRef,
    pub rows: u64,
    pub elapsed_ms: u64,
    pub notes: Vec<String>,
    pub renames: Vec<Rename>,
}

/// Removes the run from `RUNNING` however the clone ends.
struct Registered(String);

impl Drop for Registered {
    fn drop(&mut self) {
        running().remove(&self.0);
    }
}

/// Clone a table (collection…) in its own database. Progress:
/// `clone-table-progress` events `{ runId, event, … }` (`phase`, `progress`,
/// `log`). Whatever fails, the clone is dropped; the original is only read.
#[tauri::command(rename_all = "camelCase")]
pub async fn clone_table(app: AppHandle, state: State<'_, AppState>, args: CloneArgs) -> CommandResult<CloneResult> {
    if args.run_id.is_empty() || args.run_id.len() > 100 {
        return Err(CommandError::BadRequest("identificador de clonado inválido".into()));
    }
    let driver = crate::commands::schema::driver_of(&state, &args.connection_id)?.clone();
    let control = CloneControl::default();
    {
        let mut r = running();
        if r.contains_key(&args.run_id) {
            return Err(CommandError::BadRequest("ese clonado ya está en curso".into()));
        }
        r.insert(args.run_id.clone(), control.clone());
    }
    let _registered = Registered(args.run_id.clone());
    let endpoints = Arc::new(AppEndpoints {
        state: state.inner().clone(),
        run_id: args.run_id.clone(),
        connection_id: args.connection_id.clone(),
        database: args.database.clone(),
        driver,
        seq: AtomicU64::new(0),
    });
    let req = CloneRequest {
        source: args.object,
        new_name: args.new_name,
        options: CloneOptions { with_data: args.with_data, with_indexes: args.with_indexes },
    };
    let run_id = args.run_id.clone();
    let emit = move |e: CloneEvent| {
        let mut v = serde_json::to_value(&e).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.insert("runId".into(), json!(run_id));
        }
        let _ = app.emit("clone-table-progress", v);
    };
    let r = run_clone(endpoints, req, &control, emit).await?;
    Ok(CloneResult { table: r.table, rows: r.rows, elapsed_ms: r.elapsed_ms, notes: r.notes, renames: r.renames })
}

#[derive(Deserialize)]
pub struct CancelArgs {
    pub run_id: String,
}

/// Stop a clone: what it created is dropped. `false`: it isn't running.
#[tauri::command(rename_all = "camelCase")]
pub async fn clone_table_cancel(args: CancelArgs) -> CommandResult<bool> {
    Ok(match running().get(&args.run_id) {
        Some(c) => {
            c.cancel();
            true
        }
        None => false,
    })
}

//! Several DBine windows in one process (docs: "Nueva ventana").
//!
//! The first window keeps the label `main` (from `tauri.conf.json`); each new
//! one takes the lowest free `win-N` (N ≥ 2), so the window-state file stays
//! bounded and a reopened `win-2` comes back where it was left.
//!
//! One window is the primary: the only one that restores and saves the open
//! tabs and the AI conversation. When it closes and others remain, the most
//! recently focused one is promoted (`window-role` event).
//!
//! Menu clicks, the quit request and the MCP approval prompt go to one window
//! only, the target: the focused one, else the last focused, else the
//! primary, else any.

use crate::error::{CommandError, CommandResult};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, Window, WindowEvent};

/// The label of the window `tauri.conf.json` creates.
pub const MAIN: &str = "main";
/// Prefix of the windows opened later (`win-2`, `win-3`…).
const PREFIX: &str = "win-";
/// How far a new window is moved from the one it opens over.
const CASCADE: f64 = 30.0;

/// Window labels, most recently focused first.
static MRU: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The primary window's label; `None` means `main`.
static PRIMARY: Mutex<Option<String>> = Mutex::new(None);
/// Taken by the first window that asks (`app_claim_startup`).
static STARTUP_CLAIMED: AtomicBool = AtomicBool::new(false);
/// The window whose quit or close flow is running (`quit_begin`): one at a
/// time across the app, so two windows never ask at once.
static QUIT_OWNER: Mutex<Option<String>> = Mutex::new(None);
/// The window opened last: the next one cascades from it when none has focus.
static LAST_OPENED: Mutex<Option<String>> = Mutex::new(None);
/// A window being destroyed right now: `target_window` skips it.
static CLOSING: Mutex<Option<String>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn primary_label() -> String {
    lock(&PRIMARY).clone().unwrap_or_else(|| MAIN.to_string())
}

/// The lowest `win-N` (N ≥ 2) no open window uses.
fn free_label(taken: &HashSet<String>) -> String {
    (2u32..).map(|n| format!("{PREFIX}{n}")).find(|l| !taken.contains(l)).expect("unbounded range")
}

/// Order of windows in lists: `main` first, then `win-N` by N.
fn label_rank(label: &str) -> u32 {
    if label == MAIN {
        1
    } else {
        label.strip_prefix(PREFIX).and_then(|n| n.parse().ok()).unwrap_or(u32::MAX)
    }
}

/// Move `label` to the front of the MRU list.
fn touch(mru: &mut Vec<String>, label: &str) {
    mru.retain(|l| l != label);
    mru.insert(0, label.to_string());
}

/// The window that takes over from `gone`: the most recently focused one
/// still open, else the first open one.
fn successor(mru: &[String], open: &[String], gone: &str) -> Option<String> {
    mru.iter()
        .find(|l| *l != gone && open.contains(l))
        .or_else(|| open.iter().filter(|l| *l != gone).min_by_key(|l| label_rank(l)))
        .cloned()
}

/// Open a new, empty window. It starts hidden: its UI shows it once rendered.
pub fn open_new(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let taken: HashSet<String> = app.webview_windows().into_keys().collect();
    let label = free_label(&taken);
    // No primary left (it closed with no other window to promote, e.g. a
    // Dock reopen afterwards): the new window takes over, or nobody would
    // save the tabs.
    if !taken.contains(&primary_label()) {
        *lock(&PRIMARY) = Some(label.clone());
    }
    // Cascade from the focused window, else from the one opened last (with
    // no focus, e.g. from the Dock menu, two new windows would otherwise
    // land on the same spot), else from the target.
    let windows = app.webview_windows();
    let anchor = windows
        .values()
        .find(|w| w.is_focused().unwrap_or(false))
        .or_else(|| lock(&LAST_OPENED).as_deref().and_then(|l| windows.get(l)))
        .cloned()
        .or_else(|| target_window(app));
    let win = WebviewWindowBuilder::new(app, &label, WebviewUrl::default())
        .title("DBine")
        .inner_size(1400.0, 880.0)
        .min_inner_size(640.0, 400.0)
        .visible(false)
        .background_color(tauri::window::Color(0x1e, 0x1e, 0x1e, 0xff))
        .disable_drag_drop_handler()
        .build()?;
    // A label opened for the first time would land exactly over the window
    // it was opened from: move it down and to the right.
    if !has_saved_state(app, &label) {
        if let Some(anchor) = anchor {
            if let (Ok(pos), Ok(scale)) = (anchor.outer_position(), anchor.scale_factor()) {
                let d = (CASCADE * scale) as i32;
                let _ = win.set_position(tauri::PhysicalPosition::new(pos.x + d, pos.y + d));
            }
        }
    }
    *lock(&LAST_OPENED) = Some(label.clone());
    tracing::info!(%label, "window opened");
    watch_show(&win);
    Ok(win)
}

/// Open a new window off the calling thread: on Windows, building a window
/// from an event handler on the main thread deadlocks (WebView2).
pub fn spawn_new(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = open_new(&app) {
            tracing::warn!("opening a window: {e}");
        }
    });
}

/// Whether the window-state plugin remembers a geometry for `label`.
fn has_saved_state(app: &AppHandle, label: &str) -> bool {
    use tauri_plugin_window_state::AppHandleExt;
    let Ok(dir) = app.path().app_config_dir() else { return false };
    std::fs::read(dir.join(app.filename()))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&b).ok())
        .is_some_and(|m| m.contains_key(label))
}

/// The window starts hidden (no blank page while the UI loads) and the UI
/// shows it once rendered; if that never happens (a broken frontend), show it
/// anyway so the window isn't invisible.
pub fn watch_show(w: &WebviewWindow) {
    let w = w.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(4));
        if !w.is_visible().unwrap_or(true) {
            tracing::warn!(label = w.label(), "UI did not show the window — showing it");
            let _ = w.show();
        }
    });
}

/// The window menu clicks, the quit request and prompts go to.
pub fn target_window(app: &AppHandle) -> Option<WebviewWindow> {
    let mut windows = app.webview_windows();
    if let Some(gone) = lock(&CLOSING).as_deref() {
        windows.remove(gone);
    }
    if let Some(w) = windows.values().find(|w| w.is_focused().unwrap_or(false)) {
        return Some(w.clone());
    }
    let mru = lock(&MRU).clone();
    mru.iter()
        .chain(std::iter::once(&primary_label()))
        .find_map(|l| windows.get(l))
        .or_else(|| windows.values().min_by_key(|w| label_rank(w.label())))
        .cloned()
}

/// Bring a window to the front (restoring it if minimized), so a prompt
/// sent to it is seen. A window still hidden is loading: its UI shows it.
pub fn bring_forward(w: &WebviewWindow) {
    if w.is_minimized().unwrap_or(false) {
        let _ = w.unminimize();
    } else if !w.is_visible().unwrap_or(true) {
        return;
    }
    let _ = w.set_focus();
}

/// Dock click with no visible window (macOS): bring back the last used one,
/// or open a new one if there is none.
#[cfg(target_os = "macos")]
pub fn reopen(app: &AppHandle) {
    match target_window(app) {
        Some(w) => bring_forward(&w),
        None => spawn_new(app),
    }
}

/// The window whose quit or close prompt is open, if it still exists.
fn quit_owner(app: &AppHandle) -> Option<WebviewWindow> {
    let owner = lock(&QUIT_OWNER).clone()?;
    app.get_webview_window(&owner)
}

/// A quit from outside the UI (the OS, the last window going away): ask the
/// target window's UI, which confirms and calls `quit_app`. If a quit or
/// close prompt is already open, bring that window forward instead of asking
/// twice. False when there is no window to ask.
pub fn request_quit(app: &AppHandle) -> bool {
    if let Some(w) = quit_owner(app) {
        bring_forward(&w);
        return true;
    }
    let Some(w) = target_window(app) else { return false };
    bring_forward(&w);
    let _ = app.emit_to(w.label(), "quit-requested", ());
    true
}

/// Whether quitting needs the UI: some window has running tasks, or a quit
/// or close prompt is open. Otherwise an OS quit goes straight through.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn quit_needs_ui(app: &AppHandle) -> bool {
    quit_owner(app).is_some()
        || app.try_state::<TaskRegistry>().is_some_and(|r| !r.is_empty())
        || crate::commands::projects::has_unsaved(app)
}

/// Focus tracking, and cleanup when a window goes away.
pub fn on_window_event(w: &Window, e: &WindowEvent) {
    match e {
        WindowEvent::Focused(true) => touch(&mut lock(&MRU), w.label()),
        WindowEvent::Destroyed => {
            let label = w.label().to_string();
            let app = w.app_handle();
            lock(&MRU).retain(|l| *l != label);
            if let Some(reg) = app.try_state::<TaskRegistry>() {
                reg.remove(&label);
            }
            {
                let mut owner = lock(&QUIT_OWNER);
                if owner.as_deref() == Some(label.as_str()) {
                    *owner = None;
                }
            }
            if label == primary_label() {
                let open: Vec<String> = app.webview_windows().into_keys().filter(|l| *l != label).collect();
                let mru = lock(&MRU).clone();
                if let Some(next) = successor(&mru, &open, &label) {
                    *lock(&PRIMARY) = Some(next.clone());
                    tracing::info!(from = %label, to = %next, "primary window promoted");
                    let _ = app.emit_to(next.as_str(), "window-role", serde_json::json!({ "primary": true }));
                }
            }
            // A pending MCP approval shown in this window moves to another.
            *lock(&CLOSING) = Some(label.clone());
            if let Some(mcp) = app.try_state::<crate::mcp::McpRuntime>() {
                mcp.reannounce_approvals(app);
            }
            *lock(&CLOSING) = None;
        }
        _ => {}
    }
}

/// A second launch (Windows taskbar task, Linux desktop action, a second
/// double click): the single-instance plugin hands its arguments here, and
/// it opens a window in this process. `--new-window` and no arguments do the
/// same.
#[cfg_attr(any(target_os = "macos", debug_assertions), allow(dead_code))]
pub fn handle_second_launch(app: &AppHandle, argv: Vec<String>) {
    tracing::info!(?argv, "second launch: opening a window");
    spawn_new(app);
}

/// A background task as the UI shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
}

/// A running task and the window it belongs to.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RunningTask {
    pub label: String,
    pub id: String,
    pub title: String,
}

/// Each window's running background tasks, as its UI reports them: quitting
/// asks once about all of them.
#[derive(Default)]
pub struct TaskRegistry(Mutex<HashMap<String, Vec<TaskSummary>>>);

impl TaskRegistry {
    fn report(&self, label: &str, tasks: Vec<TaskSummary>) {
        let mut m = lock(&self.0);
        if tasks.is_empty() {
            m.remove(label);
        } else {
            m.insert(label.to_string(), tasks);
        }
    }

    fn remove(&self, label: &str) {
        lock(&self.0).remove(label);
    }

    fn is_empty(&self) -> bool {
        lock(&self.0).is_empty()
    }

    /// Every running task, `main`'s first, then by window number.
    fn all(&self) -> Vec<RunningTask> {
        let m = lock(&self.0);
        let mut labels: Vec<&String> = m.keys().collect();
        labels.sort_by_key(|l| (label_rank(l), l.as_str()));
        labels
            .into_iter()
            .flat_map(|l| {
                m[l].iter().map(move |t| RunningTask { label: l.clone(), id: t.id.clone(), title: t.title.clone() })
            })
            .collect()
    }
}

/// Opens a new window; returns its label.
#[tauri::command]
pub async fn window_new(app: AppHandle) -> CommandResult<String> {
    let w = open_new(&app).map_err(|e| CommandError::Internal(e.to_string()))?;
    Ok(w.label().to_string())
}

#[derive(Debug, Serialize)]
pub struct WindowRole {
    pub label: String,
    /// Restores and saves the tabs and the AI conversation.
    pub primary: bool,
    pub window_count: u32,
}

/// Who the calling window is.
#[tauri::command]
pub fn window_role(app: AppHandle, window: WebviewWindow) -> WindowRole {
    WindowRole {
        label: window.label().to_string(),
        primary: window.label() == primary_label(),
        window_count: app.webview_windows().len() as u32,
    }
}

/// Closes the calling window (not the app): its geometry is saved first,
/// while it still exists.
#[tauri::command]
pub async fn window_close(app: AppHandle, window: WebviewWindow) -> CommandResult<()> {
    use tauri_plugin_window_state::{AppHandleExt, StateFlags};
    if let Err(e) = app.save_window_state(StateFlags::all() - StateFlags::VISIBLE) {
        tracing::warn!("saving the window state on close: {e}");
    }
    window.destroy().map_err(|e| CommandError::Internal(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct TasksReportArgs {
    pub tasks: Vec<TaskSummary>,
}

/// The calling window's running tasks (the whole list, each time it changes).
#[tauri::command(rename_all = "camelCase")]
pub fn tasks_report(
    app: AppHandle,
    window: WebviewWindow,
    registry: tauri::State<'_, TaskRegistry>,
    args: TasksReportArgs,
) {
    // A report arriving after the window is gone would leave tasks nobody
    // can cancel in the quit prompt.
    if app.get_webview_window(window.label()).is_some() {
        registry.report(window.label(), args.tasks);
    }
}

/// Every window's running tasks.
#[tauri::command]
pub fn tasks_running_all(registry: tauri::State<'_, TaskRegistry>) -> Vec<RunningTask> {
    registry.all()
}

/// Asks every window to cancel its tasks (quitting with tasks running).
#[tauri::command]
pub fn tasks_cancel_all_broadcast(app: AppHandle) {
    let _ = app.emit("tasks-cancel-all", ());
}

/// Starts a quit or a window close from the calling window's UI. False when
/// another window's prompt is already open (that window comes forward): the
/// caller drops its request. Ends with `quit_end`, or when the window closes.
#[tauri::command]
pub fn quit_begin(window: WebviewWindow) -> bool {
    let app = window.app_handle();
    let busy = {
        let mut owner = lock(&QUIT_OWNER);
        let other = owner.as_deref().filter(|l| *l != window.label()).and_then(|l| app.get_webview_window(l));
        if other.is_none() {
            *owner = Some(window.label().to_string());
        }
        other
    };
    match busy {
        Some(w) => {
            bring_forward(&w);
            false
        }
        None => true,
    }
}

/// The calling window's quit or close flow ended without closing it.
#[tauri::command]
pub fn quit_end(window: WebviewWindow) {
    let mut owner = lock(&QUIT_OWNER);
    if owner.as_deref() == Some(window.label()) {
        *owner = None;
    }
}

/// True only for the first caller in this process: the once-per-run work
/// (update check, app-start telemetry) runs in one window.
#[tauri::command]
pub fn app_claim_startup() -> bool {
    !STARTUP_CLAIMED.swap(true, Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(labels: &[&str]) -> HashSet<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn new_windows_take_the_lowest_free_number() {
        assert_eq!(free_label(&set(&["main"])), "win-2");
        assert_eq!(free_label(&set(&["main", "win-2"])), "win-3");
        // A closed window's number is reused.
        assert_eq!(free_label(&set(&["main", "win-3"])), "win-2");
        assert_eq!(free_label(&set(&["win-2", "win-3", "win-5"])), "win-4");
        assert_eq!(free_label(&set(&[])), "win-2");
    }

    #[test]
    fn ranks_main_first_then_by_number() {
        let mut l = vec!["win-10", "win-2", "main", "win-3"];
        l.sort_by_key(|l| label_rank(l));
        assert_eq!(l, ["main", "win-2", "win-3", "win-10"]);
    }

    #[test]
    fn mru_moves_to_front() {
        let mut mru = Vec::new();
        touch(&mut mru, "main");
        touch(&mut mru, "win-2");
        touch(&mut mru, "main");
        assert_eq!(mru, ["main", "win-2"]);
    }

    #[test]
    fn the_last_focused_open_window_takes_over() {
        let open = vec!["win-2".to_string(), "win-3".to_string()];
        let mru = vec!["main".to_string(), "win-3".to_string(), "win-2".to_string()];
        assert_eq!(successor(&mru, &open, "main").as_deref(), Some("win-3"));
        // Never focused: the lowest-numbered open window.
        assert_eq!(successor(&[], &open, "main").as_deref(), Some("win-2"));
        assert_eq!(successor(&mru, &[], "main"), None);
    }

    fn task(id: &str) -> TaskSummary {
        TaskSummary { id: id.into(), title: format!("Tarea {id}") }
    }

    #[test]
    fn registry_lists_every_window_and_forgets_closed_ones() {
        let r = TaskRegistry::default();
        r.report("win-2", vec![task("b")]);
        r.report("main", vec![task("a1"), task("a2")]);
        let all = r.all();
        let ids: Vec<(&str, &str)> = all.iter().map(|t| (t.label.as_str(), t.id.as_str())).collect();
        assert_eq!(ids, [("main", "a1"), ("main", "a2"), ("win-2", "b")]);
        assert_eq!(all[2].title, "Tarea b");

        // An empty report clears the window's entry.
        r.report("main", vec![]);
        assert_eq!(r.all().len(), 1);
        r.remove("win-2");
        assert!(r.all().is_empty());
    }

    #[test]
    fn reads_the_ui_report() {
        let args: TasksReportArgs =
            serde_json::from_value(serde_json::json!({ "tasks": [{ "id": "t1", "title": "Exportar" }] })).unwrap();
        assert_eq!(args.tasks, [TaskSummary { id: "t1".into(), title: "Exportar".into() }]);
    }
}

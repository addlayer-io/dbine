//! `dbine --run-task <id>`: the OS scheduler starts the app this way. It
//! runs the task with no window and exits; nothing of the app's start runs
//! (single instance, windows, updater, MCP server, telemetry, cloud sync).
//!
//! Exit codes: 0 it worked (alerts included), 1 it failed or ended with
//! errors, 2 it couldn't start (no such task, the state didn't open).

use crate::state::AppState;
use dbine_core::tasks::RunStatus;
use dbine_core::StateStore;
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Reading the saved passwords waits at most this long: macOS asks the
/// user after an update changes the app's signature, and nobody may be
/// there to answer.
const SECRETS_LIMIT: Duration = Duration::from_secs(45);

const IDENTIFIER: &str = "com.addlayer.dbine";

/// The task id when the process was started as `--run-task <id>`.
pub fn requested() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--run-task" {
            return args.next();
        }
        if let Some(id) = a.strip_prefix("--run-task=") {
            return Some(id.to_string());
        }
    }
    None
}

/// Where the app keeps its state (as Tauri's `app_config_dir`) and its
/// downloaded components (`app_data_dir`).
fn dirs() -> Option<(PathBuf, PathBuf)> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let (config, data) = {
        let base = home?.join("Library/Application Support");
        (base.clone(), base)
    };
    #[cfg(windows)]
    let (config, data) = {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| home.map(|h| h.join("AppData/Roaming")))?;
        (base.clone(), base)
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let (config, data) = (
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home.clone().map(|h| h.join(".config")))?,
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| home.map(|h| h.join(".local/share")))?,
    );
    Some((config.join(IDENTIFIER), data.join(IDENTIFIER)))
}

/// Run the task and return the exit code.
pub fn run(task_id: &str) -> i32 {
    let Some((dir, data_dir)) = dirs() else {
        eprintln!("dbine: no se encontró la carpeta del usuario");
        return 2;
    };
    let logs_dir = dir.join("logs");
    let _ = std::fs::create_dir_all(&logs_dir);
    let _ = crate::LOG_DIR.set(logs_dir.clone());
    let (writer, guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(&logs_dir, "app.log"));
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(writer))
        .try_init();
    let code = run_logged(task_id, &dir, &data_dir);
    drop(guard);
    code
}

fn run_logged(task_id: &str, dir: &std::path::Path, data_dir: &std::path::Path) -> i32 {
    dbine_core::secrets::set_vault_path(dir.join("dbine-secrets.vault"));
    dbine_driver::runtime::set_components_dir(data_dir.join("components"));
    let store = match StateStore::open(&dir.join("dbine-state.sqlite")) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(task = task_id, "scheduled task: state unavailable: {e}");
            return 2;
        }
    };
    let state = AppState::new(store);
    let task = match crate::tasks::load(&state, task_id) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(task = task_id, "scheduled task: {e}");
            return 2;
        }
    };
    if !task.enabled {
        tracing::info!(task = task_id, "scheduled task disabled: not run");
        return 0;
    }
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("scheduled task: no runtime: {e}");
            return 2;
        }
    };
    let run = rt.block_on(async {
        // The saved passwords first, each with a time limit: a keychain
        // prompt nobody answers must fail the run, not hang it.
        for conn in crate::tasks::connections_of(&task) {
            let saved = state.store.get_connection(&conn).ok().flatten();
            if !saved.as_ref().is_some_and(|c| c.save_password) {
                continue;
            }
            let id = conn.clone();
            match tokio::time::timeout(SECRETS_LIMIT, tokio::task::spawn_blocking(move || dbine_core::secrets::get(&id))).await {
                Ok(Ok(Ok(_))) => {}
                Ok(Ok(Err(e))) => return Err(keychain_message(&e.to_string())),
                Ok(Err(e)) => return Err(e.to_string()),
                Err(_) => return Err(keychain_message("no respondió")),
            }
        }
        // The mail server's password, the same way.
        if task.steps.iter().any(|s| s.kind == dbine_core::tasks::kinds::SEND_MAIL) {
            let read = tokio::task::spawn_blocking(|| dbine_core::secrets::get_raw(crate::tasks::mail::PASSWORD));
            match tokio::time::timeout(SECRETS_LIMIT, read).await {
                Ok(Ok(Ok(_))) => {}
                Ok(Ok(Err(e))) => return Err(keychain_message(&e.to_string())),
                Ok(Err(e)) => return Err(e.to_string()),
                Err(_) => return Err(keychain_message("no respondió")),
            }
        }
        Ok(crate::tasks::run_task(&state, None, &task, "schedule").await)
    });
    let code = match run {
        Ok(run) => match run.status {
            RunStatus::Ok => 0,
            _ => 1,
        },
        Err(msg) => {
            // Recorded as a run, so the app's history shows why.
            let failed = dbine_core::tasks::TaskRun {
                id: uuid::Uuid::new_v4().to_string(),
                task_id: task.id.clone(),
                trigger: "schedule".into(),
                status: RunStatus::Failed,
                started_at: dbine_core::tasks::now_text(),
                finished_at: dbine_core::tasks::now_text(),
                error: Some(msg.clone()),
                ..Default::default()
            };
            let _ = state.store.save_task_run(&failed);
            tracing::error!(task = task_id, "scheduled task failed before starting: {msg}");
            if task.notify != dbine_core::tasks::Notify::Never {
                crate::tasks::notify::send(&format!("DBine · {}", task.name), &msg);
            }
            1
        }
    };
    // A keychain call still blocked (the prompt): don't wait for it.
    rt.shutdown_timeout(Duration::from_secs(2));
    code
}

fn keychain_message(e: &str) -> String {
    format!(
        "No se pudo leer la contraseña guardada en el llavero del sistema ({e}). \
         Abrí DBine una vez y permití el acceso al llavero («Permitir siempre»): después de una actualización el sistema vuelve a pedirlo."
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn config_dir_ends_with_identifier() {
        let (config, data) = super::dirs().unwrap();
        assert!(config.ends_with(super::IDENTIFIER));
        assert!(data.ends_with(super::IDENTIFIER));
    }
}

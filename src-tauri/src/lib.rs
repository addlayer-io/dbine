mod commands;
mod dbdocs;
#[cfg(debug_assertions)]
mod devtools;
#[cfg(target_os = "macos")]
mod dock_macos;
mod error;
#[cfg(windows)]
mod jumplist_windows;
mod lint;
mod mcp;
mod menu;
mod optimizer;
mod state;
mod sync;
mod tasks;
mod tunnels;
mod windows;

use crate::state::AppState;
use dbine_core::StateStore;
use std::io::Write;
use tauri::Manager;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Set once the UI has confirmed quitting (no background tasks, or the user
/// chose to cancel them): from then on an exit request goes through.
static QUIT_CONFIRMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Quits the app after the UI's quit guard: saves the state of every window
/// (still alive here, so maximized/fullscreen are kept too) and exits.
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    prepare_exit(&app);
    app.exit(0);
}

/// The UI's quit guard went through (quitting, or restarting to finish an
/// update): let the exit request pass and save every window's state while
/// the windows are still alive.
pub(crate) fn prepare_exit(app: &tauri::AppHandle) {
    use tauri_plugin_window_state::AppHandleExt;
    QUIT_CONFIRMED.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Err(e) = app.save_window_state(
        tauri_plugin_window_state::StateFlags::all() - tauri_plugin_window_state::StateFlags::VISIBLE,
    ) {
        tracing::warn!("saving the window state on quit: {e}");
    }
}

/// The exit didn't happen after all (an update that failed to install):
/// later exit requests go through the quit guard again.
pub(crate) fn cancel_exit() {
    QUIT_CONFIRMED.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// What must not outlive the process: on `RunEvent::Exit`, and before the
/// Windows updater ends the process itself (it never reaches that event).
pub(crate) fn exit_cleanup(app: &tauri::AppHandle) {
    // The built-in model's llama-server must not outlive the app.
    dbine_ai::embedded::shutdown();
    // Profilers put server settings back (a few seconds at most).
    let state = app.state::<AppState>();
    // The timeout's timer must be created inside the runtime.
    tauri::async_runtime::block_on(async {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), commands::profiler::stop_all(&state)).await;
    });
}

/// Resolved at startup; used by the panic hook + the `get_log_dir` command.
/// `OnceLock` because tauri's setup() is the first place we know the path.
pub(crate) static LOG_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn log_dir() -> Option<&'static std::path::Path> {
    LOG_DIR.get().map(|p| p.as_path())
}

/// Append a single line directly to `app.log` (bypassing tracing). Used by
/// the panic hook so a crash logs even if the subscriber is mid-flush.
fn append_app_log_raw(line: &str) {
    let Some(dir) = LOG_DIR.get() else { return };
    let _ = std::fs::create_dir_all(dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("app.log")) {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Before any driver opens a TLS connection (see Cargo.toml).
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Started by the OS scheduler: run the task with no window and exit,
    // before anything of the app starts (tasks/headless.rs).
    if let Some(id) = tasks::headless::requested() {
        std::process::exit(tasks::headless::run(&id));
    }
    let builder = tauri::Builder::default();
    // First: a second launch (Jump List task, desktop action, a second
    // double click) only opens a window in this process. macOS's
    // LaunchServices already keeps one instance. Not in debug builds: a
    // `cargo tauri dev` run must not hand itself over to an installed release.
    #[cfg(all(any(windows, target_os = "linux"), not(debug_assertions)))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
        windows::handle_second_launch(app, argv)
    }));
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // In-app updates; only Rust calls it (commands/updates.rs), the
        // webview has no updater permission.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // The menu bar (macOS): the UI sends it (`app_menu_set`) and gets its clicks.
        .on_menu_event(menu::on_event)
        // Focus order (for the target window), primary window, tasks.
        .on_window_event(windows::on_window_event)
        // Size, position and maximized state across launches. Not the
        // visibility: the window starts hidden and the UI shows it.
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all() - tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )
        .setup(|app| {
            // 1. Where state + logs live (same directory).
            let dir = app.path().app_config_dir().unwrap_or_else(|_| std::env::temp_dir().join("dbine"));
            let _ = std::fs::create_dir_all(&dir);
            let state_path = dir.join("dbine-state.sqlite");
            // Connection secrets: encrypted file next to the state, its key in the keychain.
            dbine_core::secrets::set_vault_path(dir.join("dbine-secrets.vault"));
            let logs_dir = dir.join("logs");
            let _ = std::fs::create_dir_all(&logs_dir);
            let _ = LOG_DIR.set(logs_dir.clone());

            // 2. Tracing to stdout (for `cargo tauri dev`) and to a daily
            //    rolling file (so a crashed GUI app leaves evidence behind).
            let (file_writer, file_guard) =
                tracing_appender::non_blocking(tracing_appender::rolling::daily(&logs_dir, "app.log"));
            // Keep the guard for the whole process: dropping it would close
            // the writer mid-line.
            Box::leak(Box::new(file_guard));
            let _ = tracing_subscriber::registry()
                // The updater plugin logs a missing latest.json (releases up
                // to 0.1.3) as an ERROR; commands/updates.rs already logs
                // every check failure, with the GitHub fallback.
                .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tauri_plugin_updater=off")))
                .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(file_writer))
                .with(tracing_subscriber::fmt::layer())
                .try_init();

            // 3. Panic hook: the default message goes to stderr, invisible
            //    in a GUI build; mirror it to app.log.
            let prev_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
                append_app_log_raw(&format!("{ts} [PANIC] {info}"));
                tracing::error!(panic = %info, "process panic");
                prev_hook(info);
            }));

            tracing::info!(?state_path, ?logs_dir, "starting dbine");

            // 4. State store.
            let store = StateStore::open(&state_path).expect("open state store");
            // Every window reloads what another one (or MCP, or a restore)
            // changed.
            let handle = app.handle().clone();
            store.set_change_hook(Box::new(move |change| {
                use tauri::Emitter;
                let _ = handle.emit("state-changed", change);
            }));
            let state = AppState::new(store);
            // 4c. The explorer's cache, next to the state (not part of it:
            //     never synced, rebuilt if lost; docs/cache-del-explorador.md).
            match dbine_core::ExplorerCache::open(&dir.join("cache.db")) {
                Ok(c) => {
                    let _ = state.cache.set(c);
                }
                Err(e) => tracing::warn!("explorer cache unavailable: {e}"),
            }
            // 5. Cloud backup: its manager and the background task.
            let _ = state.sync.set(sync::SyncManager::new(state.clone(), &dir, &app.config().identifier));
            // 4b. The local MCP server (off unless the user turned it on; docs/mcp.md).
            app.manage(mcp::McpRuntime::open(state.clone(), &dir));
            app.state::<mcp::McpRuntime>().attach(app.handle().clone());
            // The OS scheduler's entries follow the app (moved, updated).
            #[cfg(not(debug_assertions))]
            {
                let state = state.clone();
                std::thread::spawn(move || commands::scheduled::resync_all(&state));
            }
            app.manage(state);
            state::set_app_handle(app.handle().clone());
            app.manage(windows::TaskRegistry::default());
            app.manage(commands::projects::UnsavedRegistry::default());
            sync::start(app.handle().clone());
            // 6. AI assistant; the built-in model's files go in `models/`.
            let data_dir = app.path().app_data_dir().unwrap_or_else(|_| dir.clone());
            app.manage(commands::ai::AiRuntime::new(data_dir.join("models")));
            // Migrations: the bulk transfer engine's state next to the app's
            // (runs cut by the last exit become resumable).
            app.manage(commands::migration::MigrationRuns::open(&dir));
            // 7. Engine libraries downloaded on first use (DuckDB) go in
            //    `components/`; their progress reaches the UI as an event.
            dbine_driver::runtime::set_components_dir(data_dir.join("components"));
            let handle = app.handle().clone();
            dbine_driver::runtime::set_progress_sink(move |p| {
                use tauri::Emitter;
                let _ = handle.emit("component-download", p);
            });
            // Downloadable drivers: newer versions published apart from
            // the app (`drivers-changed` refreshes Configuración → Drivers).
            commands::drivers::start_updater(app.handle().clone());

            #[cfg(debug_assertions)]
            devtools::start(app.handle().clone());

            if let Some(win) = app.get_webview_window(windows::MAIN) {
                windows::watch_show(&win);
            }
            // "Nueva ventana" in the Dock menu (macOS) and the taskbar Jump
            // List (Windows); Linux's is in the .desktop file.
            #[cfg(target_os = "macos")]
            dock_macos::install(app.handle().clone());
            #[cfg(windows)]
            jumplist_windows::install("Nueva ventana");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            quit_app,
            windows::window_new,
            windows::window_role,
            windows::window_close,
            windows::tasks_report,
            windows::tasks_running_all,
            windows::tasks_cancel_all_broadcast,
            windows::app_claim_startup,
            windows::quit_begin,
            windows::quit_end,
            #[cfg(debug_assertions)]
            devtools::dev_report,
            commands::health::health,
            commands::health::get_log_dir,
            commands::health::log_ui_error,
            commands::health::save_text_file,
            commands::health::save_binary_file,
            commands::connections::list_drivers,
            commands::connections::list_connections,
            commands::connections::save_connection,
            commands::connections::delete_connection,
            commands::connections::trust_ssh_host,
            commands::history::history_list,
            commands::history::history_of,
            commands::history::history_delete,
            commands::data_compare::data_compare,
            commands::data_compare::data_compare_script,
            commands::security::security_principals,
            commands::security::security_grants,
            commands::security::security_script,
            commands::backup::backup_list,
            commands::backup::backup_script,
            commands::backup::backup_default_path,
            commands::backup::backup_copy,
            commands::backup::backup_copy_delete,
            commands::connections::test_connection,
            commands::connections::connect,
            commands::connections::disconnect,
            commands::folders::list_folders,
            commands::folders::save_folder,
            commands::folders::delete_folder,
            commands::folders::move_connection,
            commands::folders::reorder_explorer,
            commands::compare::schema_compare_load,
            commands::compare::schema_compare,
            commands::compare::schema_compare_convert,
            commands::compare::schema_sync_script,
            commands::compare::schema_sync_run,
            commands::conn_import::import_connections_detect,
            commands::conn_import::import_connections_scan,
            commands::conn_import::import_connections_apply,
            commands::explorer::list_databases,
            commands::explorer::list_objects,
            commands::explorer::list_database_objects,
            commands::explorer::get_permissions,
            commands::explorer::get_index_usage,
            commands::explorer::get_dependents,
            commands::explorer::index_toggle_script,
            commands::rename::rename_impact,
            commands::rename::rename_script,
            commands::explorer::get_cached,
            commands::explorer::scan_keys,
            commands::explorer::get_columns,
            commands::explorer::get_definition,
            commands::explorer::browse_query,
            commands::explorer::filtered_browse_query,
            commands::query::execute_query,
            commands::export::export_rows_to_file,
            commands::export::export_query_to_file,
            commands::schema::database_schema,
            commands::schema::table_ddl,
            commands::schema::insert_script,
            commands::schema::update_script,
            commands::library::list_library,
            commands::library::save_library_script,
            commands::library::delete_library_script,
            commands::library::import_library_files,
            commands::library::export_library,
            commands::library_git::library_git_status,
            commands::library_git::library_git_link,
            commands::library_git::library_git_unlink,
            commands::library_git::library_git_commit,
            commands::library_git::library_git_pull,
            commands::library_git::library_git_push,
            commands::library_git::library_git_sync,
            commands::library_git::library_git_resolve,
            commands::projects::list_projects,
            commands::projects::project_default_dir,
            commands::projects::project_inspect_folder,
            commands::projects::project_link,
            commands::projects::project_clone,
            commands::projects::project_update,
            commands::projects::project_relocate,
            commands::projects::project_unlink,
            commands::projects::project_set_binding,
            commands::projects::project_reorder,
            commands::projects::project_write_manifest,
            commands::projects::project_reveal,
            commands::projects::project_set_remote,
            commands::projects::project_set_identity,
            commands::projects::project_list_dir,
            commands::projects::project_read_file,
            commands::projects::project_write_file,
            commands::projects::project_stat_files,
            commands::projects::project_create_file,
            commands::projects::project_create_dir,
            commands::projects::project_rename,
            commands::projects::project_delete,
            commands::projects_git::project_status,
            commands::projects_git::project_diff,
            commands::projects_git::project_file_log,
            commands::projects_git::project_file_at,
            commands::projects_git::project_commit,
            commands::projects_git::project_pull,
            commands::projects_git::project_push,
            commands::projects_git::project_sync,
            commands::projects_git::project_conflict,
            commands::projects_git::project_operation,
            commands::projects_git::project_discard,
            commands::projects_git::project_git_cancel,
            commands::projects::files_report,
            commands::projects::files_unsaved_all,
            commands::projects::files_save_all_broadcast,
            commands::migration::migration_targets,
            commands::migration::migration_plan,
            commands::migration::migration_run,
            commands::migration::migration_set_parallel,
            commands::migration::migration_cancel_table,
            commands::migration::migration_run_now,
            commands::migration::migration_cancel,
            commands::migration::migration_runs,
            commands::migration::migration_resume,
            commands::migration::migration_retry_failed,
            commands::migration::migration_forget,
            commands::clone_table::clone_table,
            commands::clone_table::clone_table_cancel,
            commands::schema::create_database,
            commands::schema::create_database_script,
            commands::schema::create_database_choices,
            commands::schema::database_properties,
            commands::schema::alter_database_script,
            commands::schema::alter_database,
            commands::search::search_database,
            commands::optimizer::optimizer_analyze,
            commands::optimizer::optimizer_ai,
            commands::optimizer::optimizer_compare,
            commands::optimizer::optimizer_cancel,
            commands::lint::lint_script,
            commands::lint::lint_rules,
            commands::query_builder::build_query,
            commands::query_builder::preview_built_query,
            commands::datagen::datagen_preview,
            commands::datagen::datagen_run,
            commands::subset::subset_plan,
            commands::subset::subset_run,
            commands::db_health::database_health,
            commands::dbdocs::dbdocs_outline,
            commands::dbdocs::dbdocs_generate,
            commands::dbdocs::dbdocs_open,
            commands::scheduled::scheduled_tasks_list,
            commands::scheduled::scheduled_task_check,
            commands::scheduled::scheduled_task_save,
            commands::scheduled::scheduled_task_delete,
            commands::scheduled::scheduled_task_enable,
            commands::scheduled::scheduled_task_run_now,
            commands::scheduled::scheduled_task_runs,
            commands::mail::mail_settings_get,
            commands::mail::mail_settings_save,
            commands::mail::mail_test,
            commands::schema::drop_database,
            commands::schema::drop_objects,
            commands::schemas::schema_spec,
            commands::schemas::create_schema_script,
            commands::schemas::drop_schema_script,
            commands::schemas::schema_object_count,
            commands::scripts::generate_script,
            commands::scripts::run_script_file,
            commands::imports::preview_import_file,
            commands::imports::import_file,
            commands::query::cancel_query,
            commands::query::close_session,
            commands::query::set_tab_autocommit,
            commands::query::commit_tab,
            commands::query::rollback_tab,
            commands::query::tab_transaction_state,
            commands::query::split_script,
            commands::multi_db::run_multi_db,
            commands::multi_db::cancel_multi_db,
            commands::queries::list_queries,
            commands::queries::get_query,
            commands::queries::save_query,
            commands::queries::query_versions,
            commands::queries::query_version,
            commands::queries::query_version_checkpoint,
            commands::queries::delete_query,
            commands::saved_migrations::list_saved_migrations,
            commands::saved_migrations::get_saved_migration,
            commands::saved_migrations::save_saved_migration,
            commands::saved_migrations::rename_saved_migration,
            commands::saved_migrations::link_saved_migration_run,
            commands::saved_migrations::duplicate_saved_migration,
            commands::saved_migrations::delete_saved_migration,
            commands::settings::list_settings,
            commands::settings::set_setting,
            commands::settings::open_support_page,
            mcp::commands::mcp_status,
            mcp::commands::mcp_configure,
            mcp::commands::mcp_create_client,
            mcp::commands::mcp_revoke_client,
            mcp::commands::mcp_activity,
            mcp::commands::mcp_pending_approvals,
            mcp::commands::mcp_answer_approval,
            mcp::commands::mcp_clear_approve_all,
            menu::app_menu_set,
            commands::telemetry::track_event,
            commands::updates::check_for_update,
            commands::updates::open_release_page,
            commands::updates::update_download,
            commands::updates::update_cancel,
            commands::updates::update_install_and_restart,
            commands::drivers::drivers_packages,
            commands::drivers::drivers_install,
            commands::drivers::drivers_remove,
            commands::drivers::drivers_check_updates,
            commands::drivers::drivers_rollback,
            commands::sync::sync_status,
            commands::sync::sync_connect,
            commands::sync::sync_cancel_connect,
            commands::sync::sync_setup,
            commands::sync::sync_now,
            commands::sync::sync_upload_now,
            commands::sync::sync_restore_now,
            commands::sync::sync_set_auto,
            commands::sync::sync_set_passphrase,
            commands::sync::sync_change_passphrase,
            commands::sync::sync_disconnect,
            commands::sync::sync_local_backups,
            commands::sync::sync_restore_local,
            commands::monitor::monitor_snapshot,
            commands::monitor::monitor_blocking,
            commands::monitor::monitor_kill_session,
            commands::monitor::monitor_processes,
            commands::monitor::monitor_cancel_query,
            commands::profiler::profiler_start,
            commands::profiler::profiler_poll,
            commands::profiler::profiler_stop,
            commands::ai::ai_detect,
            commands::ai::ai_chat,
            commands::ai::ai_approve,
            commands::ai::ai_cancel,
            commands::ai::ai_download_model,
            commands::ai::ai_delete_model,
            commands::ai::ai_start_ollama,
            commands::ai::ai_pull_ollama,
        ])
        .build(tauri::generate_context!())
        .expect("error building tauri app")
        .run(|app, event| {
            // The last window went away without the UI's quit guard: the
            // target window's UI asks first when background tasks (of any
            // window) are running, then calls `quit_app`. `app.exit(n)` comes
            // with a code and goes through; with no window left there's no
            // one to ask. (An OS quit on macOS — Dock › Salir, app switcher,
            // logout — never gets here: `dock_macos` answers
            // `applicationShouldTerminate:`.)
            if let tauri::RunEvent::ExitRequested { code: None, api, .. } = &event {
                if !QUIT_CONFIRMED.load(std::sync::atomic::Ordering::SeqCst) && windows::request_quit(app) {
                    api.prevent_exit();
                }
            }
            // Dock click with every window minimized or hidden (macOS).
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { has_visible_windows: false, .. } = &event {
                windows::reopen(app);
            }
            if let tauri::RunEvent::Exit = event {
                exit_cleanup(app);
            }
        });
}

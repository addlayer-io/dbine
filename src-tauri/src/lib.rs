mod commands;
#[cfg(debug_assertions)]
mod devtools;
mod error;
mod mcp;
mod menu;
mod state;
mod sync;
mod tunnels;

use crate::state::AppState;
use dbine_core::StateStore;
use std::io::Write;
use tauri::Manager;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Resolved at startup; used by the panic hook + the `get_log_dir` command.
/// `OnceLock` because tauri's setup() is the first place we know the path.
static LOG_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

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
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // The menu bar (macOS): the UI sends it (`app_menu_set`) and gets its clicks.
        .on_menu_event(menu::on_event)
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
                .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
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
            app.manage(state);
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

            // The window starts hidden (no blank page while the UI loads) and
            // the UI shows it once rendered; if that never happens (a broken
            // frontend), show it anyway so the app isn't invisible.
            #[cfg(debug_assertions)]
            devtools::start(app.handle().clone());

            if let Some(win) = app.get_webview_window("main") {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(4));
                    if !win.is_visible().unwrap_or(true) {
                        tracing::warn!("UI did not show the window — showing it");
                        let _ = win.show();
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
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
            commands::queries::list_queries,
            commands::queries::get_query,
            commands::queries::save_query,
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
            commands::drivers::drivers_packages,
            commands::drivers::drivers_install,
            commands::drivers::drivers_remove,
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
            commands::profiler::profiler_start,
            commands::profiler::profiler_poll,
            commands::profiler::profiler_stop,
            commands::ai::ai_detect,
            commands::ai::ai_chat,
            commands::ai::ai_cancel,
            commands::ai::ai_download_model,
            commands::ai::ai_delete_model,
            commands::ai::ai_start_ollama,
            commands::ai::ai_pull_ollama,
        ])
        .build(tauri::generate_context!())
        .expect("error building tauri app")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // The built-in model's llama-server must not outlive the app.
                dbine_ai::embedded::shutdown();
                // Profilers put server settings back (a few seconds at most).
                let state = app.state::<AppState>();
                // The timeout's timer must be created inside the runtime.
                tauri::async_runtime::block_on(async {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), commands::profiler::stop_all(&state)).await;
                });
            }
        });
}

//! The downloadable drivers (release builds with the `plugins` feature):
//! which ones there are, which are on disk, download and remove, look for
//! newer versions and go back to the previous one. A build without the
//! feature carries every driver inside: the list is empty and the settings
//! page says so.

use crate::error::{CommandError, CommandResult};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct DriverPackage {
    pub package: String,
    pub label: String,
    /// The version in use (or the one a download would get).
    pub version: String,
    pub drivers: Vec<String>,
    pub size: u64,
    pub installed: Option<u64>,
    /// The newest version this app can run.
    pub available: String,
    /// What "Volver a la anterior" goes back to.
    pub previous: Option<String>,
    /// `{kind: up_to_date | downloading | ready_next_connection |
    /// needs_app (min_app) | rolled_back (from, reason) |
    /// restart_for_new_options}`.
    pub status: serde_json::Value,
    pub min_app_needed: Option<String>,
}

#[derive(Serialize)]
pub struct DriverPackages {
    /// False when every driver comes inside the app (nothing to download).
    pub on_demand: bool,
    pub packages: Vec<DriverPackage>,
}

#[derive(Deserialize)]
pub struct PackageArgs {
    pub package: String,
}

#[cfg(feature = "plugins")]
#[tauri::command]
pub async fn drivers_packages() -> CommandResult<DriverPackages> {
    let packages = dbine_drivers::plugins::packages()
        .into_iter()
        .map(|p| DriverPackage {
            status: serde_json::to_value(&p.status).unwrap_or_default(),
            package: p.package,
            label: p.label,
            version: p.version,
            drivers: p.drivers,
            size: p.size,
            installed: p.installed,
            available: p.available,
            previous: p.previous,
            min_app_needed: p.min_app_needed,
        })
        .collect();
    Ok(DriverPackages { on_demand: true, packages })
}

#[cfg(not(feature = "plugins"))]
#[tauri::command]
pub async fn drivers_packages() -> CommandResult<DriverPackages> {
    Ok(DriverPackages { on_demand: false, packages: Vec::new() })
}

#[cfg(feature = "plugins")]
fn known(package: &str) -> CommandResult<()> {
    if dbine_drivers::plugins::packages().iter().any(|p| p.package == package) {
        Ok(())
    } else {
        Err(CommandError::BadRequest(format!("no hay un driver descargable '{package}'")))
    }
}

#[cfg(feature = "plugins")]
#[tauri::command(rename_all = "camelCase")]
pub async fn drivers_install(args: PackageArgs) -> CommandResult<()> {
    known(&args.package)?;
    Ok(dbine_drivers::plugins::install(&args.package).await?)
}

#[cfg(not(feature = "plugins"))]
#[tauri::command(rename_all = "camelCase")]
pub async fn drivers_install(args: PackageArgs) -> CommandResult<()> {
    let _ = args;
    Err(CommandError::BadRequest("esta versión trae todos los drivers incluidos".into()))
}

/// A running host keeps its file open on Windows: removing it fails until
/// its connections are closed.
#[cfg(feature = "plugins")]
#[tauri::command(rename_all = "camelCase")]
pub async fn drivers_remove(args: PackageArgs) -> CommandResult<()> {
    known(&args.package)?;
    dbine_drivers::plugins::remove(&args.package)
        .map_err(|e| CommandError::Internal(format!("no se pudo borrar el driver (¿tiene conexiones abiertas?): {e}")))
}

#[cfg(not(feature = "plugins"))]
#[tauri::command(rename_all = "camelCase")]
pub async fn drivers_remove(args: PackageArgs) -> CommandResult<()> {
    let _ = args;
    Err(CommandError::BadRequest("esta versión trae todos los drivers incluidos".into()))
}

/// "Buscar actualizaciones": fetch the drivers index now. Newer versions of
/// the installed drivers download in the background (`drivers-changed`
/// tells the UI how they go).
#[cfg(feature = "plugins")]
#[tauri::command]
pub async fn drivers_check_updates() -> CommandResult<()> {
    dbine_drivers::plugins::check_updates().await.map_err(CommandError::Internal)
}

#[cfg(not(feature = "plugins"))]
#[tauri::command]
pub async fn drivers_check_updates() -> CommandResult<()> {
    Ok(())
}

/// "Volver a la anterior": the version in use is dropped (never picked
/// again) and new connections use the one before it.
#[cfg(feature = "plugins")]
#[tauri::command(rename_all = "camelCase")]
pub async fn drivers_rollback(args: PackageArgs) -> CommandResult<()> {
    known(&args.package)?;
    dbine_drivers::plugins::rollback(&args.package).map_err(CommandError::BadRequest)
}

#[cfg(not(feature = "plugins"))]
#[tauri::command(rename_all = "camelCase")]
pub async fn drivers_rollback(args: PackageArgs) -> CommandResult<()> {
    let _ = args;
    Err(CommandError::BadRequest("esta versión trae todos los drivers incluidos".into()))
}

/// Start the drivers' update check and forward their changes to the UI.
#[cfg(feature = "plugins")]
pub fn start_updater(app: tauri::AppHandle) {
    use tauri::Emitter;
    dbine_drivers::plugins::on_change(move || {
        let _ = app.emit("drivers-changed", ());
    });
    tauri::async_runtime::spawn(dbine_drivers::plugins::run_updater());
}

#[cfg(not(feature = "plugins"))]
pub fn start_updater(app: tauri::AppHandle) {
    let _ = app;
}

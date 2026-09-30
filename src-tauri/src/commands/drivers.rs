//! The downloadable drivers (release builds with the `plugins` feature):
//! which ones there are, which are on disk, download and remove. A build
//! without the feature carries every driver inside: the list is empty and
//! the settings page says so.

use crate::error::{CommandError, CommandResult};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct DriverPackage {
    pub package: String,
    pub label: String,
    pub version: String,
    pub drivers: Vec<String>,
    pub size: u64,
    pub installed: Option<u64>,
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
        .map(|p| DriverPackage { package: p.package, label: p.label, version: p.version, drivers: p.drivers, size: p.size, installed: p.installed })
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

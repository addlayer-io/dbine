//! Import connections from DBeaver and DbGate: find their files, list what's
//! there (without secrets) and save the chosen ones. Passwords stay in the
//! backend: they go from the other tool's file straight to the keychain.

use crate::error::{CommandError, CommandResult};
use crate::state::{driver_info, extract_secrets, AppState};
use dbine_core::conn_import::{self, Candidate, Source};
use dbine_core::{secrets, ConnectionFolder, SavedConnection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tauri::State;

/// Where each tool keeps its connections on this machine (`null` when it
/// isn't there).
#[tauri::command]
pub async fn import_connections_detect() -> CommandResult<HashMap<Source, Option<String>>> {
    Ok([Source::Dbeaver, Source::Dbgate, Source::Datagrip, Source::AzureDataStudio, Source::Ssms]
        .into_iter()
        .map(|s| (s, conn_import::default_location(s).map(|p| p.display().to_string())))
        .collect())
}

#[derive(Deserialize)]
pub struct ScanArgs {
    pub source: Source,
    /// A file or folder; the tool's usual place when missing.
    pub path: Option<String>,
    /// For `url`: the pasted lines.
    pub text: Option<String>,
}

#[derive(Serialize)]
pub struct ScanItem {
    pub key: String,
    pub name: String,
    pub folder: Vec<String>,
    pub color: Option<String>,
    pub source_kind: String,
    pub driver: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: Option<String>,
    pub has_secret: bool,
    pub tags: Vec<String>,
    /// The password is in the other tool's keychain entry (read on import).
    pub keychain: bool,
    pub notes: Vec<String>,
    pub unsupported: Option<String>,
    /// A DBine connection that already points to the same place.
    pub existing: Option<String>,
}

#[derive(Serialize)]
pub struct ScanResult {
    pub path: String,
    pub items: Vec<ScanItem>,
    pub warnings: Vec<String>,
}

fn location(source: Source, path: Option<String>) -> CommandResult<PathBuf> {
    match path.filter(|p| !p.trim().is_empty()) {
        Some(p) => Ok(PathBuf::from(p)),
        None => conn_import::default_location(source)
            .ok_or_else(|| CommandError::BadRequest(format!("no se encontraron conexiones de {} en esta máquina: elegí la carpeta o el archivo", source.label()))),
    }
}

/// Candidates whose engine isn't in this build count as unsupported.
fn read(source: Source, path: Option<String>, text: Option<String>) -> CommandResult<conn_import::Found> {
    let mut found = if source == Source::Url {
        conn_import::read_text(text.as_deref().unwrap_or(""))
    } else {
        conn_import::read(source, &location(source, path)?)?
    };
    for c in &mut found.candidates {
        if c.unsupported.is_none() && driver_info(&c.config.driver).is_err() {
            c.unsupported = Some(format!("el driver «{}» no está en esta versión de DBine", c.config.driver));
        }
    }
    Ok(found)
}

fn same_place(a: &dbine_driver::ConnectionConfig, b: &dbine_driver::ConnectionConfig) -> bool {
    a.driver == b.driver
        && a.host.eq_ignore_ascii_case(&b.host)
        && a.port == b.port
        && a.database.eq_ignore_ascii_case(&b.database)
        && a.username.as_deref().unwrap_or("") == b.username.as_deref().unwrap_or("")
}

#[tauri::command(rename_all = "camelCase")]
pub async fn import_connections_scan(state: State<'_, AppState>, args: ScanArgs) -> CommandResult<ScanResult> {
    let found = read(args.source, args.path, args.text)?;
    let saved = state.store.list_connections()?;
    let items = found
        .candidates
        .iter()
        .map(|c: &Candidate| ScanItem {
            key: c.key.clone(),
            name: c.name.clone(),
            folder: c.folder.clone(),
            color: c.color.clone(),
            source_kind: c.source_kind.clone(),
            driver: c.config.driver.clone(),
            host: c.config.host.clone(),
            port: c.config.port,
            database: c.config.database.clone(),
            username: c.config.username.clone(),
            has_secret: c.has_secret(),
            tags: c.tags.clone(),
            keychain: c.keychain.is_some() && !c.has_secret(),
            notes: c.notes.clone(),
            unsupported: c.unsupported.clone(),
            existing: saved.iter().find(|s| same_place(&s.config, &c.config)).map(|s| s.name.clone()),
        })
        .collect();
    Ok(ScanResult { path: found.path.display().to_string(), items, warnings: found.warnings })
}

#[derive(Deserialize)]
pub struct ApplyArgs {
    pub source: Source,
    /// What the scan read (`path` of its result, or the pasted `text`).
    pub path: Option<String>,
    pub text: Option<String>,
    /// The `key`s to import.
    pub keys: Vec<String>,
    /// Keep the passwords in the keychain; otherwise they're asked on connect.
    pub passwords: bool,
}

#[derive(Serialize)]
pub struct ApplyResult {
    pub imported: usize,
    pub folders: usize,
    /// `[name, error]`.
    pub failed: Vec<(String, String)>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn import_connections_apply(state: State<'_, AppState>, args: ApplyArgs) -> CommandResult<ApplyResult> {
    let found = read(args.source, args.path, args.text)?;
    // Existing folders by (parent, lowercase name), so imports land in them.
    let mut folders: HashMap<(Option<String>, String), String> =
        state.store.list_folders()?.into_iter().map(|f| ((f.parent_id.clone(), f.name.to_lowercase()), f.id)).collect();
    let mut new_folders = 0;
    let mut out = ApplyResult { imported: 0, folders: 0, failed: Vec::new() };
    for c in found.candidates.into_iter().filter(|c| args.keys.contains(&c.key)) {
        if let Some(why) = &c.unsupported {
            out.failed.push((c.name.clone(), why.clone()));
            continue;
        }
        let mut parent: Option<String> = None;
        for name in &c.folder {
            let k = (parent.clone(), name.to_lowercase());
            let id = match folders.get(&k) {
                Some(id) => id.clone(),
                None => {
                    let f = state.store.save_folder(&ConnectionFolder { id: uuid::Uuid::new_v4().to_string(), name: name.clone(), parent_id: parent.clone(), color: None })?;
                    new_folders += 1;
                    folders.insert(k, f.id.clone());
                    f.id
                }
            };
            parent = Some(id);
        }
        let result = (|| -> CommandResult<()> {
            let info = driver_info(&c.config.driver)?;
            let mut config = c.config;
            // From the other tool's keychain entry (the system may ask first).
            if args.passwords && config.password.is_none() {
                config.password = c.keychain.as_ref().and_then(|k| k.fetch());
            }
            let secret = extract_secrets(&mut config, info);
            let id = uuid::Uuid::new_v4().to_string();
            let keep = args.passwords && !secret.is_empty();
            if keep {
                secrets::set(&id, &secret)?;
            }
            let conn = SavedConnection { id, name: c.name.clone(), color: c.color, config, save_password: keep, folder_id: parent, tags: c.tags.clone(), mcp_level: None, updated_at: String::new() };
            state.store.save_connection(&conn)?;
            Ok(())
        })();
        match result {
            Ok(()) => out.imported += 1,
            Err(e) => out.failed.push((c.name, e.to_string())),
        }
    }
    out.folders = new_folders;
    Ok(out)
}

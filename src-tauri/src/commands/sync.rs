//! Cloud backup commands (docs/sync.md, docs/api-commands.md).

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use crate::sync::{RunStatus, SyncConfig, SyncManager};
use dbine_sync::engine::LocalBackup;
use dbine_sync::{ProviderKind, SyncAction, BACKUP_FILE, PREVIOUS_FILE};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// Shortest passphrase accepted for a new backup.
const MIN_PASSPHRASE: usize = 10;

fn mgr<'a>(state: &'a State<'_, AppState>) -> CommandResult<&'a SyncManager> {
    state.sync.get().ok_or_else(|| CommandError::Internal("sincronización no iniciada".into()))
}

#[derive(Serialize)]
pub struct ProviderInfo {
    pub kind: ProviderKind,
    pub label: &'static str,
    /// This build has the provider's app registration.
    pub available: bool,
}

#[derive(Serialize)]
pub struct SyncStatusOut {
    pub config: SyncConfig,
    pub providers: Vec<ProviderInfo>,
    pub status: RunStatus,
    /// Local changes not uploaded yet.
    pub dirty: bool,
    pub last_sync_at: Option<String>,
}

#[tauri::command]
pub async fn sync_status(state: State<'_, AppState>) -> CommandResult<SyncStatusOut> {
    let m = mgr(&state)?;
    Ok(SyncStatusOut {
        config: m.config(),
        providers: [ProviderKind::GoogleDrive, ProviderKind::Onedrive, ProviderKind::Folder]
            .into_iter()
            .map(|kind| ProviderInfo {
                kind,
                label: match kind {
                    ProviderKind::Folder => "Carpeta",
                    k => k.label(),
                },
                available: m.available(kind),
            })
            .collect(),
        status: m.status.lock().unwrap().clone(),
        dirty: m.engine.is_dirty().unwrap_or(false),
        last_sync_at: m.engine.last_sync_at(),
    })
}

#[derive(Deserialize)]
pub struct ConnectArgs {
    pub provider: ProviderKind,
    #[serde(default)]
    pub folder: Option<String>,
}

#[derive(Serialize)]
pub struct RemoteInfo {
    pub updated_at: String,
    pub device: String,
    pub app_version: String,
}

#[derive(Serialize)]
pub struct ConnectOut {
    pub account: String,
    /// The backup already stored there, if any (another machine's).
    pub remote: Option<RemoteInfo>,
}

/// Sign in (or pick the folder) and look for an existing backup. Nothing
/// syncs until `sync_setup`.
#[tauri::command(rename_all = "camelCase")]
pub async fn sync_connect(app: AppHandle, state: State<'_, AppState>, args: ConnectArgs) -> CommandResult<ConnectOut> {
    let m = mgr(&state)?;
    if !m.available(args.provider) {
        return Err(CommandError::BadRequest(format!(
            "esta versión de DBine no tiene configurado el acceso a {} (falta registrar la app: ver docs/sync.md)",
            args.provider.label()
        )));
    }
    let prev = m.config();
    if args.provider != ProviderKind::Folder {
        m.sign_in(&app, args.provider).await?;
    }
    let mut cfg = SyncConfig {
        provider: Some(args.provider),
        folder: args.folder.filter(|f| !f.trim().is_empty()),
        account: None,
        enabled: false,
        auto: prev.auto || prev.provider.is_none(),
    };
    let cloud = m.cloud(&cfg)?;
    cfg.account = Some(cloud.account().await?);
    let remote = m.engine.remote_header(cloud.as_ref()).await?.map(|h| RemoteInfo {
        updated_at: h.updated_at,
        device: h.device,
        app_version: h.app_version,
    });
    // Pointed somewhere else: what was synced before doesn't apply.
    m.engine.reset()?;
    m.save_config(&cfg)?;
    Ok(ConnectOut { account: cfg.account.unwrap_or_default(), remote })
}

#[tauri::command]
pub async fn sync_cancel_connect(state: State<'_, AppState>) -> CommandResult<()> {
    mgr(&state)?.cancel_sign_in();
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupMode {
    /// This machine's state becomes the backup (a new one, or replacing
    /// the stored one, which is kept as the previous).
    Upload,
    /// The stored backup replaces this machine's state.
    Restore,
}

#[derive(Deserialize)]
pub struct SetupArgs {
    pub passphrase: String,
    pub mode: SetupMode,
}

/// Finish setting up: first upload or restore, then keep the passphrase in
/// the keychain and turn sync on.
#[tauri::command(rename_all = "camelCase")]
pub async fn sync_setup(app: AppHandle, state: State<'_, AppState>, args: SetupArgs) -> CommandResult<SyncAction> {
    let m = mgr(&state)?;
    let pass = args.passphrase;
    let engine = &m.engine;
    let action = match args.mode {
        SetupMode::Upload => {
            if pass.chars().count() < MIN_PASSPHRASE {
                return Err(CommandError::BadRequest(format!("la frase clave tiene que tener al menos {MIN_PASSPHRASE} caracteres")));
            }
            m.run_with(&app, Some(pass.clone()), |cloud, p| async move { engine.push(cloud.as_ref(), &p, true).await }).await?
        }
        SetupMode::Restore => m.run_with(&app, Some(pass.clone()), |cloud, p| async move { engine.pull(cloud.as_ref(), &p).await }).await?,
    };
    m.store_passphrase(&pass)?;
    let mut cfg = m.config();
    cfg.enabled = true;
    m.save_config(&cfg)?;
    Ok(action)
}

/// Sync now (what the automatic sync does).
#[tauri::command]
pub async fn sync_now(app: AppHandle, state: State<'_, AppState>) -> CommandResult<SyncAction> {
    let m = mgr(&state)?;
    let engine = &m.engine;
    m.run(&app, |cloud, p| async move { engine.sync(cloud.as_ref(), &p).await }).await
}

/// Upload this machine's state now, replacing the stored backup.
#[tauri::command]
pub async fn sync_upload_now(app: AppHandle, state: State<'_, AppState>) -> CommandResult<SyncAction> {
    let m = mgr(&state)?;
    let engine = &m.engine;
    m.run(&app, |cloud, p| async move { engine.push(cloud.as_ref(), &p, false).await }).await
}

/// Replace this machine's state with the stored backup.
#[tauri::command]
pub async fn sync_restore_now(app: AppHandle, state: State<'_, AppState>) -> CommandResult<SyncAction> {
    let m = mgr(&state)?;
    let engine = &m.engine;
    m.run(&app, |cloud, p| async move { engine.pull(cloud.as_ref(), &p).await }).await
}

#[derive(Deserialize)]
pub struct AutoArgs {
    pub auto: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sync_set_auto(state: State<'_, AppState>, args: AutoArgs) -> CommandResult<()> {
    let m = mgr(&state)?;
    let mut cfg = m.config();
    cfg.auto = args.auto;
    m.save_config(&cfg)
}

#[derive(Deserialize)]
pub struct PassphraseArgs {
    pub passphrase: String,
}

/// Type the passphrase on this machine (it was changed on another one, or
/// the keychain lost it): checked against the stored backup first.
#[tauri::command(rename_all = "camelCase")]
pub async fn sync_set_passphrase(state: State<'_, AppState>, args: PassphraseArgs) -> CommandResult<()> {
    let m = mgr(&state)?;
    let cloud = m.cloud(&m.config())?;
    m.engine.verify(cloud.as_ref(), &args.passphrase).await?;
    m.store_passphrase(&args.passphrase)
}

#[derive(Deserialize)]
pub struct ChangePassphraseArgs {
    pub current: String,
    pub new: String,
}

/// Re-encrypt the backup with a new passphrase (other machines will ask
/// for it).
#[tauri::command(rename_all = "camelCase")]
pub async fn sync_change_passphrase(app: AppHandle, state: State<'_, AppState>, args: ChangePassphraseArgs) -> CommandResult<SyncAction> {
    let m = mgr(&state)?;
    if m.passphrase()?.as_deref() != Some(args.current.as_str()) {
        return Err(CommandError::WrongPassphrase("la frase clave actual no es correcta".into()));
    }
    if args.new.chars().count() < MIN_PASSPHRASE {
        return Err(CommandError::BadRequest(format!("la frase clave tiene que tener al menos {MIN_PASSPHRASE} caracteres")));
    }
    let engine = &m.engine;
    let action = m.run_with(&app, Some(args.new.clone()), |cloud, p| async move { engine.push(cloud.as_ref(), &p, true).await }).await?;
    m.store_passphrase(&args.new)?;
    Ok(action)
}

#[derive(Deserialize)]
pub struct DisconnectArgs {
    /// Also delete the backup from the cloud / folder.
    #[serde(default)]
    pub delete_remote: bool,
}

/// Turn sync off on this machine: forget the account and the passphrase
/// (and, if asked, delete the stored backup). Local data stays.
#[tauri::command(rename_all = "camelCase")]
pub async fn sync_disconnect(state: State<'_, AppState>, args: DisconnectArgs) -> CommandResult<()> {
    let m = mgr(&state)?;
    let cfg = m.config();
    if args.delete_remote && cfg.provider.is_some() {
        let cloud = m.cloud(&cfg)?;
        cloud.delete(BACKUP_FILE).await?;
        cloud.delete(PREVIOUS_FILE).await?;
    }
    m.forget_credentials();
    m.engine.reset()?;
    m.save_config(&SyncConfig::default())?;
    *m.status.lock().unwrap() = RunStatus::default();
    Ok(())
}

#[tauri::command]
pub async fn sync_local_backups(state: State<'_, AppState>) -> CommandResult<Vec<LocalBackup>> {
    Ok(mgr(&state)?.engine.local_backups())
}

#[derive(Deserialize)]
pub struct RestoreLocalArgs {
    pub path: String,
}

/// Put back a copy saved before a restore; the next sync uploads it.
#[tauri::command(rename_all = "camelCase")]
pub async fn sync_restore_local(app: AppHandle, state: State<'_, AppState>, args: RestoreLocalArgs) -> CommandResult<()> {
    let m = mgr(&state)?;
    let pass = m.passphrase()?.ok_or_else(|| CommandError::WrongPassphrase("falta la frase clave en esta máquina".into()))?;
    m.engine.restore_local(&args.path, &pass)?;
    for c in state.store.list_connections()? {
        state.close_connection_sessions(&c.id);
    }
    let _ = app.emit("sync-applied", ());
    Ok(())
}

//! Cloud backup in the app (docs/sincronizacion.md): which provider and
//! account, the passphrase and tokens in the keychain, and the background
//! task that syncs after local changes (debounced) and every few minutes
//! (to pick up other machines' changes).

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::secrets::{self, Secrets};
use dbine_sync::folder::FolderStore;
use dbine_sync::gdrive::GoogleDrive;
use dbine_sync::oauth::{self, ClientIds, TokenSource, Tokens};
use dbine_sync::onedrive::OneDrive;
use dbine_sync::{CloudStore, ProviderKind, SecretsIo, SyncAction, SyncEngine};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

const K_CONFIG: &str = "local.sync.config";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    pub provider: Option<ProviderKind>,
    /// The folder, for the `folder` provider.
    #[serde(default)]
    pub folder: Option<String>,
    /// Who's signed in (shown in the UI).
    #[serde(default)]
    pub account: Option<String>,
    /// Set up (passphrase chosen, first sync done).
    #[serde(default)]
    pub enabled: bool,
    /// Sync by itself after changes and on start.
    #[serde(default = "yes")]
    pub auto: bool,
}

fn yes() -> bool {
    true
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self { provider: None, folder: None, account: None, enabled: false, auto: true }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunStatus {
    pub running: bool,
    pub last_error: Option<String>,
    pub last_error_kind: Option<String>,
    pub last_action: Option<SyncAction>,
    pub last_run_at: Option<String>,
}

/// Connections' secrets through the app's keychain cache.
struct KeychainIo(AppState);

impl SecretsIo for KeychainIo {
    fn read(&self, id: &str) -> dbine_core::Result<Secrets> {
        if let Some(s) = self.0.keychain_cache.get(id) {
            return Ok(s.clone());
        }
        let s = secrets::get(id)?;
        self.0.keychain_cache.insert(id.into(), s.clone());
        Ok(s)
    }
    fn write(&self, id: &str, s: &Secrets) -> dbine_core::Result<()> {
        secrets::set(id, s)?;
        self.0.keychain_cache.insert(id.into(), s.clone());
        Ok(())
    }
    fn remove(&self, id: &str) -> dbine_core::Result<()> {
        self.0.keychain_cache.remove(id);
        secrets::delete(id)
    }
}

pub struct SyncManager {
    pub engine: SyncEngine,
    /// Keychain names are prefixed with the app's identifier, so a test
    /// build never touches the real app's passphrase or tokens.
    kc_prefix: String,
    state: AppState,
    ids: ClientIds,
    http: reqwest::Client,
    pub status: Mutex<RunStatus>,
    /// One sync at a time.
    run_lock: tokio::sync::Mutex<()>,
    passphrase: Mutex<Option<String>>,
    tokens: Arc<Mutex<HashMap<String, Tokens>>>,
    login_cancel: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

/// This machine's name, written in the backup's header ("from MacBook").
fn device_name() -> String {
    if let Ok(n) = std::env::var("COMPUTERNAME") {
        return n;
    }
    #[cfg(target_os = "macos")]
    if let Ok(o) = std::process::Command::new("scutil").args(["--get", "ComputerName"]).output() {
        let n = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !n.is_empty() {
            return n;
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "equipo".into())
}

impl SyncManager {
    pub fn new(state: AppState, config_dir: &std::path::Path, identifier: &str) -> Self {
        // Client IDs: build-time, overridable by `cloud-clients.json` next to
        // the state file.
        let file: ClientIds = std::fs::read(config_dir.join("cloud-clients.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let ids = ClientIds::from_build().overridden_by(file);
        let engine = SyncEngine::new(
            state.store.clone(),
            Arc::new(KeychainIo(state.clone())),
            device_name(),
            env!("CARGO_PKG_VERSION").into(),
            config_dir.join("backups"),
        );
        Self {
            engine,
            kc_prefix: identifier.to_string(),
            state,
            ids,
            http: reqwest::Client::builder().timeout(Duration::from_secs(120)).build().unwrap_or_default(),
            status: Mutex::new(RunStatus::default()),
            run_lock: tokio::sync::Mutex::new(()),
            passphrase: Mutex::new(None),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            login_cancel: Mutex::new(None),
        }
    }

    pub fn config(&self) -> SyncConfig {
        self.state
            .store
            .get_setting(K_CONFIG)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    pub fn save_config(&self, c: &SyncConfig) -> CommandResult<()> {
        Ok(self.state.store.set_setting(K_CONFIG, Some(&serde_json::to_value(c).map_err(|e| CommandError::Internal(e.to_string()))?))?)
    }

    /// Which providers this build can use (OAuth ones need their client ID).
    pub fn available(&self, kind: ProviderKind) -> bool {
        kind == ProviderKind::Folder || self.ids.config(kind).is_some()
    }

    fn kc_passphrase(&self) -> String {
        format!("{}.sync.passphrase", self.kc_prefix)
    }

    fn kc_tokens(&self, kind: ProviderKind) -> String {
        let k = serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        format!("{}.sync.token.{k}", self.kc_prefix)
    }

    // -- passphrase ---------------------------------------------------------

    pub fn passphrase(&self) -> CommandResult<Option<String>> {
        if let Some(p) = self.passphrase.lock().unwrap().clone() {
            return Ok(Some(p));
        }
        let p = secrets::get_raw(&self.kc_passphrase())?;
        *self.passphrase.lock().unwrap() = p.clone();
        Ok(p)
    }

    pub fn store_passphrase(&self, p: &str) -> CommandResult<()> {
        secrets::set_raw(&self.kc_passphrase(), p)?;
        *self.passphrase.lock().unwrap() = Some(p.to_string());
        Ok(())
    }

    fn require_passphrase(&self) -> CommandResult<String> {
        self.passphrase()?.ok_or_else(|| {
            CommandError::WrongPassphrase("falta la frase clave del backup en esta máquina: escribila en Configuración › Sincronización".into())
        })
    }

    // -- provider -----------------------------------------------------------

    fn load_tokens(&self, kind: ProviderKind) -> CommandResult<Tokens> {
        let key = self.kc_tokens(kind);
        if let Some(t) = self.tokens.lock().unwrap().get(&key) {
            return Ok(t.clone());
        }
        let raw = secrets::get_raw(&key)?.ok_or_else(|| CommandError::SyncAuth(format!("conectá la cuenta de {}", kind.label())))?;
        let t: Tokens = serde_json::from_str(&raw).map_err(|e| CommandError::Internal(e.to_string()))?;
        self.tokens.lock().unwrap().insert(key, t.clone());
        Ok(t)
    }

    /// Refreshed tokens go to the keychain and the in-memory copy.
    fn token_saver(&self, kind: ProviderKind) -> Arc<dyn Fn(&Tokens) + Send + Sync> {
        let key = self.kc_tokens(kind);
        let cache = self.tokens.clone();
        Arc::new(move |t: &Tokens| {
            cache.lock().unwrap().insert(key.clone(), t.clone());
            if let Ok(json) = serde_json::to_string(t) {
                if let Err(e) = secrets::set_raw(&key, &json) {
                    tracing::warn!(%e, "could not store refreshed cloud tokens");
                }
            }
        })
    }

    /// The configured backup place.
    pub fn cloud(&self, cfg: &SyncConfig) -> CommandResult<Box<dyn CloudStore>> {
        let kind = cfg.provider.ok_or_else(|| CommandError::BadRequest("elegí dónde guardar el backup".into()))?;
        Ok(match kind {
            ProviderKind::Folder => {
                let dir = cfg.folder.clone().ok_or_else(|| CommandError::BadRequest("elegí la carpeta".into()))?;
                Box::new(FolderStore::new(dir))
            }
            ProviderKind::GoogleDrive | ProviderKind::Onedrive => {
                let oc = self.ids.config(kind).ok_or_else(|| not_configured(kind))?;
                let tokens = self.load_tokens(kind)?;
                let src = Arc::new(TokenSource::new(oc, self.http.clone(), tokens, self.token_saver(kind)));
                if kind == ProviderKind::GoogleDrive {
                    Box::new(GoogleDrive::new(src))
                } else {
                    Box::new(OneDrive::new(src))
                }
            }
        })
    }

    /// Sign in with the provider in the browser (OAuth) and keep its tokens.
    pub async fn sign_in(&self, app: &AppHandle, kind: ProviderKind) -> CommandResult<()> {
        let oc = self.ids.config(kind).ok_or_else(|| not_configured(kind))?;
        let login = oauth::begin(oc).await?;
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url(login.auth_url.clone(), None::<&str>)
            .map_err(|e| CommandError::Internal(format!("no se pudo abrir el navegador: {e}")))?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self.login_cancel.lock().unwrap() = Some(tx);
        let tokens = tokio::select! {
            r = login.finish(&self.http, Duration::from_secs(300)) => r?,
            _ = rx => return Err(CommandError::Cancelled),
        };
        let key = self.kc_tokens(kind);
        secrets::set_raw(&key, &serde_json::to_string(&tokens).map_err(|e| CommandError::Internal(e.to_string()))?)?;
        self.tokens.lock().unwrap().insert(key, tokens);
        Ok(())
    }

    pub fn cancel_sign_in(&self) {
        if let Some(tx) = self.login_cancel.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }

    /// Forget the account's tokens (and the passphrase) on this machine.
    pub fn forget_credentials(&self) {
        for kind in [ProviderKind::GoogleDrive, ProviderKind::Onedrive] {
            let _ = secrets::delete_raw(&self.kc_tokens(kind));
        }
        self.tokens.lock().unwrap().clear();
        let _ = secrets::delete_raw(&self.kc_passphrase());
        *self.passphrase.lock().unwrap() = None;
        self.engine.forget_key();
    }

    // -- running ------------------------------------------------------------

    /// Run one sync operation, reporting its status to the UI. After a
    /// restore, sessions of changed connections are closed and the UI is
    /// told to reload.
    pub async fn run<F, Fut>(&self, app: &AppHandle, op: F) -> CommandResult<SyncAction>
    where
        F: FnOnce(Box<dyn CloudStore>, String) -> Fut,
        Fut: std::future::Future<Output = dbine_sync::Result<SyncAction>>,
    {
        self.run_with(app, None, op).await
    }

    /// [`run`](Self::run) with a passphrase that isn't stored yet (setup).
    pub async fn run_with<F, Fut>(&self, app: &AppHandle, passphrase: Option<String>, op: F) -> CommandResult<SyncAction>
    where
        F: FnOnce(Box<dyn CloudStore>, String) -> Fut,
        Fut: std::future::Future<Output = dbine_sync::Result<SyncAction>>,
    {
        let _guard = self.run_lock.lock().await;
        let cfg = self.config();
        self.set_status(app, |s| s.running = true);
        let before: HashMap<String, String> =
            self.state.store.list_connections().unwrap_or_default().into_iter().map(|c| (c.id.clone(), c.updated_at)).collect();
        let result: CommandResult<SyncAction> = async {
            let cloud = self.cloud(&cfg)?;
            let pass = match passphrase {
                Some(p) => p,
                None => self.require_passphrase()?,
            };
            Ok(op(cloud, pass).await?)
        }
        .await;
        if let Ok(SyncAction::Downloaded { .. }) = &result {
            let after: HashMap<String, String> =
                self.state.store.list_connections().unwrap_or_default().into_iter().map(|c| (c.id.clone(), c.updated_at)).collect();
            for (id, at) in &before {
                if after.get(id) != Some(at) {
                    self.state.close_connection_sessions(id);
                    self.state.typed_secrets.remove(id);
                }
            }
            let _ = app.emit("sync-applied", ());
        }
        self.set_status(app, |s| {
            s.running = false;
            s.last_run_at = Some(chrono::Utc::now().to_rfc3339());
            match &result {
                Ok(a) => {
                    s.last_action = Some(a.clone());
                    s.last_error = None;
                    s.last_error_kind = None;
                }
                Err(e) => {
                    s.last_error = Some(e.to_string());
                    s.last_error_kind = serde_json::to_value(e).ok().and_then(|v| v.get("kind").and_then(|k| k.as_str().map(str::to_string)));
                }
            }
        });
        if let Err(e) = &result {
            tracing::warn!(error = %e, "sync failed");
        }
        result
    }

    fn set_status(&self, app: &AppHandle, f: impl FnOnce(&mut RunStatus)) {
        let snapshot = {
            let mut s = self.status.lock().unwrap();
            f(&mut s);
            s.clone()
        };
        let _ = app.emit("sync-status", snapshot);
    }
}

fn not_configured(kind: ProviderKind) -> CommandError {
    CommandError::BadRequest(format!(
        "esta versión de DBine no tiene configurado el acceso a {} (falta registrar la app: ver docs/sincronizacion.md)",
        kind.label()
    ))
}

/// The background task: sync a few seconds after local changes settle, on
/// start, and every few minutes to pick up other machines' changes. After a
/// failure it waits before retrying (longer when the user has to act).
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        const SETTLE: Duration = Duration::from_secs(4);
        const REMOTE_EVERY: Duration = Duration::from_secs(10 * 60);
        let mut seen_rev = None;
        let mut changed_at = Instant::now();
        let mut last_remote: Option<Instant> = None;
        let mut retry_at = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Some(state) = app.try_state::<AppState>() else { continue };
            let Some(mgr) = state.sync.get() else { continue };
            let cfg = mgr.config();
            if !cfg.enabled || !cfg.auto {
                last_remote = None;
                continue;
            }
            let rev = state.store.revision().ok();
            if rev != seen_rev {
                seen_rev = rev;
                changed_at = Instant::now();
            }
            let local_due = mgr.engine.is_dirty().unwrap_or(false) && changed_at.elapsed() >= SETTLE;
            let remote_due = last_remote.is_none_or(|t| t.elapsed() >= REMOTE_EVERY);
            if !(local_due || remote_due) || Instant::now() < retry_at {
                continue;
            }
            last_remote = Some(Instant::now());
            let engine = &mgr.engine;
            match mgr.run(&app, |cloud, pass| async move { engine.sync(cloud.as_ref(), &pass).await }).await {
                Ok(_) => retry_at = Instant::now(),
                Err(CommandError::WrongPassphrase(_) | CommandError::SyncAuth(_)) => retry_at = Instant::now() + Duration::from_secs(15 * 60),
                Err(_) => retry_at = Instant::now() + Duration::from_secs(60),
            }
        }
    });
}

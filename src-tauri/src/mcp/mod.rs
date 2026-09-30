//! DBine's local MCP (Model Context Protocol) server (docs/mcp.md).
//!
//! While the app is open and the user turned it on (Settings › MCP), DBine
//! answers MCP over Streamable HTTP on `127.0.0.1:<port>/mcp`. Each client
//! (Claude Code, Codex…) has its own bearer token; only its SHA-256 hash is
//! kept. What a client may do is decided per connection: `disabled`,
//! `schema` (structure only), `read` (plus read-only queries) or `write`
//! (plus writes, each one approved by the user in DBine: `approvals`).
//!
//! Everything here is local to this machine: the settings live under the
//! state store's `local.` prefix, so they never reach the cloud backup.

pub mod activity;
pub mod approvals;
pub mod commands;
mod server;
mod tools;
mod write;
#[cfg(test)]
mod tests;

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::SavedConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

pub const DEFAULT_PORT: u16 = 27517;
const CONFIG_KEY: &str = "local.mcp.config";
const CLIENTS_KEY: &str = "local.mcp.clients";

/// What MCP clients may do with a connection. Ordered: each level includes
/// the ones below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpLevel {
    /// Invisible to MCP clients.
    Disabled,
    /// Databases, objects and their structure.
    #[default]
    Schema,
    /// Also sample rows, read-only queries and plans.
    Read,
    /// Also writes, each one approved by the user in DBine.
    Write,
}

impl McpLevel {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "disabled" => Self::Disabled,
            "schema" => Self::Schema,
            "read" => Self::Read,
            "write" => Self::Write,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Schema => "schema",
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    /// The level's name as the UI shows it (Spanish), for messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "deshabilitado",
            Self::Schema => "esquema",
            Self::Read => "lectura",
            Self::Write => "escritura",
        }
    }
}

/// Whether a level can be chosen. Every level can now (`write` asks the
/// user before each write); kept as the one place to refuse one.
pub fn check_level(_level: McpLevel) -> CommandResult<()> {
    Ok(())
}

/// A connection's override as saved (`None` = the global default).
pub fn check_override(value: Option<&str>) -> CommandResult<()> {
    match value {
        None => Ok(()),
        Some(v) => check_level(McpLevel::parse(v).ok_or_else(|| CommandError::BadRequest(format!("nivel de MCP desconocido: {v}")))?),
    }
}

/// Settings › MCP.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub default_level: McpLevel,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for McpConfig {
    fn default() -> Self {
        Self { enabled: false, port: DEFAULT_PORT, default_level: McpLevel::Schema }
    }
}

/// A client allowed to connect. Its token was shown once; only the hash stays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpClient {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub created_at: String,
    #[serde(default)]
    pub last_used_at: Option<String>,
}

/// Why a connection can't go above `read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    /// Tagged `prod`.
    Prod,
    /// Configured read-only.
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    pub level: McpLevel,
    /// Set when a cap lowered the level asked for.
    pub cap: Option<Cap>,
}

pub fn is_prod(conn: &SavedConnection) -> bool {
    conn.tags.iter().any(|t| t.trim().eq_ignore_ascii_case("prod"))
}

/// The connection's override, else the global default; a `prod` tag or a
/// read-only connection caps it at `read`.
pub fn effective_level(conn: &SavedConnection, default: McpLevel) -> Effective {
    let asked = conn.mcp_level.as_deref().and_then(McpLevel::parse).unwrap_or(default);
    let cap = if asked > McpLevel::Read {
        if is_prod(conn) {
            Some(Cap::Prod)
        } else if conn.config.read_only {
            Some(Cap::ReadOnly)
        } else {
            None
        }
    } else {
        None
    };
    Effective { level: if cap.is_some() { McpLevel::Read } else { asked }, cap }
}

// -- tokens ---------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new client token: 256 random bits, hex, with a recognizable prefix.
pub fn new_token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("the OS random generator failed");
    format!("dbine_{}", hex(&b))
}

pub fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// Whether `token` is the one `hash` was made from (constant time).
pub fn verify_token(token: &str, hash: &str) -> bool {
    let h = hash_token(token);
    h.len() == hash.len() && h.bytes().zip(hash.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

// -- settings -------------------------------------------------------------------------

pub fn load_config(state: &AppState) -> McpConfig {
    state
        .store
        .get_setting(CONFIG_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn save_config(state: &AppState, cfg: &McpConfig) -> CommandResult<()> {
    Ok(state.store.set_setting(CONFIG_KEY, Some(&serde_json::to_value(cfg).map_err(|e| CommandError::Internal(e.to_string()))?))?)
}

pub fn load_clients(state: &AppState) -> Vec<McpClient> {
    state
        .store
        .get_setting(CLIENTS_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn save_clients(state: &AppState, clients: &[McpClient]) -> CommandResult<()> {
    Ok(state.store.set_setting(CLIENTS_KEY, Some(&serde_json::to_value(clients).map_err(|e| CommandError::Internal(e.to_string()))?))?)
}

// -- runtime --------------------------------------------------------------------------

/// The server and what it shares with the commands (managed by Tauri).
pub struct McpRuntime {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub state: AppState,
    pub activity: activity::ActivityLog,
    /// Writes waiting for the user's answer, and who approves everything.
    pub approvals: approvals::Approvals,
    server: Mutex<Option<Running>>,
    /// Why the server isn't running although it's on (port busy…).
    last_error: Mutex<Option<String>>,
    /// Serializes read-modify-write of the clients list.
    clients_lock: Mutex<()>,
}

struct Running {
    port: u16,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl McpRuntime {
    /// The runtime with its activity log next to the state; starts the
    /// server when it's on.
    pub fn open(state: AppState, dir: &std::path::Path) -> Self {
        let activity = activity::ActivityLog::open(&dir.join("dbine-mcp-activity.sqlite")).unwrap_or_else(|e| {
            tracing::warn!(%e, "mcp activity log unavailable, keeping it in memory");
            activity::ActivityLog::in_memory()
        });
        let rt = Self::with_log(state, activity);
        rt.apply();
        rt
    }

    pub fn with_log(state: AppState, activity: activity::ActivityLog) -> Self {
        Self {
            inner: Arc::new(Inner {
                state,
                activity,
                approvals: approvals::Approvals::default(),
                server: Mutex::new(None),
                last_error: Mutex::new(None),
                clients_lock: Mutex::new(()),
            }),
        }
    }

    /// Start, stop or move the server to match the saved settings.
    pub fn apply(&self) {
        let cfg = load_config(&self.inner.state);
        let mut server = self.inner.server.lock().unwrap_or_else(|e| e.into_inner());
        if !cfg.enabled {
            *server = None;
            *self.inner.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return;
        }
        if server.as_ref().is_some_and(|s| s.port == cfg.port) {
            return;
        }
        // A server that just stopped may hold the port for a moment.
        let restarting = server.take().is_some();
        let result = self.start(cfg.port, restarting);
        let mut err = self.inner.last_error.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(running) => {
                tracing::info!(port = running.port, "mcp server listening on 127.0.0.1");
                *server = Some(running);
                *err = None;
            }
            Err(e) => {
                tracing::warn!(%e, "mcp server not started");
                *err = Some(e);
            }
        }
    }

    /// Bind 127.0.0.1:`port` (0 = any free port, for tests) and serve.
    fn start(&self, port: u16, restarting: bool) -> Result<Running, String> {
        let mut attempt = 0;
        let bound = loop {
            match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) {
                Err(e) if restarting && e.kind() == std::io::ErrorKind::AddrInUse && attempt < 20 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                r => break r,
            }
        };
        let listener = bound.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                format!("el puerto {port} está ocupado por otro programa: elegí otro en Configuración › MCP")
            } else {
                format!("no se pudo abrir el puerto {port}: {e}")
            }
        })?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let bound = listener.local_addr().map_err(|e| e.to_string())?.port();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let inner = self.inner.clone();
        tauri::async_runtime::spawn(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(%e, "mcp listener");
                    return;
                }
            };
            let app = server::router(inner, bound);
            if let Err(e) = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await
            {
                tracing::error!(%e, "mcp server stopped");
            }
        });
        Ok(Running { port: bound, shutdown: Some(tx) })
    }

    /// The port it listens on, when running.
    pub fn running_port(&self) -> Option<u16> {
        self.inner.server.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| s.port)
    }

    pub fn last_error(&self) -> Option<String> {
        self.inner.last_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Tell the UI about pending writes (`mcp-approvals`, the whole list),
    /// and bring the window forward when one arrives, even if it was hidden
    /// or minimized.
    pub fn attach(&self, app: tauri::AppHandle) {
        use tauri::{Emitter, Manager};
        self.inner.approvals.set_sink(Arc::new(move |pending, new| {
            let _ = app.emit("mcp-approvals", pending);
            if new {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.unminimize();
                    let _ = win.set_focus();
                    let _ = win.request_user_attention(Some(tauri::UserAttentionType::Critical));
                }
            }
        }));
    }
}

impl Inner {
    /// The client a bearer token belongs to (its last use is recorded).
    pub fn authenticate(&self, token: &str) -> Option<McpClient> {
        let _guard = self.clients_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut clients = load_clients(&self.state);
        let i = clients.iter().position(|c| verify_token(token, &c.token_hash))?;
        let now = chrono::Utc::now();
        // At most one write a minute per client.
        let stale = clients[i]
            .last_used_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .is_none_or(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() >= 60);
        if stale {
            clients[i].last_used_at = Some(now.to_rfc3339());
            if let Err(e) = save_clients(&self.state, &clients) {
                tracing::warn!(%e, "mcp: could not record client use");
            }
        }
        Some(clients[i].clone())
    }

    pub fn create_client(&self, name: &str) -> CommandResult<(McpClient, String)> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CommandError::BadRequest("el cliente necesita un nombre".into()));
        }
        let _guard = self.clients_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut clients = load_clients(&self.state);
        if clients.iter().any(|c| c.name.eq_ignore_ascii_case(name)) {
            return Err(CommandError::BadRequest(format!("ya hay un cliente llamado «{name}»")));
        }
        let token = new_token();
        let client = McpClient {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            token_hash: hash_token(&token),
            created_at: chrono::Utc::now().to_rfc3339(),
            last_used_at: None,
        };
        clients.push(client.clone());
        save_clients(&self.state, &clients)?;
        Ok((client, token))
    }

    pub fn revoke_client(&self, id: &str) -> CommandResult<()> {
        let _guard = self.clients_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut clients = load_clients(&self.state);
        let before = clients.len();
        clients.retain(|c| c.id != id);
        if clients.len() == before {
            return Err(CommandError::NotFound("ese cliente ya no existe".into()));
        }
        save_clients(&self.state, &clients)?;
        self.approvals.forget_client(id);
        Ok(())
    }
}

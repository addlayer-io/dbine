use crate::error::{CommandError, CommandResult};
use dashmap::DashMap;
use dbine_core::secrets::{self, Secrets};
use dbine_core::StateStore;
use dbine_driver::{ConnectionConfig, DriverInfo, Session};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

/// A live session: one connection to one database. Explorer calls share a
/// session per database (`meta:` keys); each editor tab has its own.
pub struct SessionEntry {
    pub connection_id: String,
    pub database: String,
    pub session: Mutex<Box<dyn Session>>,
    /// Wakes the running statement's `select!` to drop it.
    pub cancel: Notify,
    /// Set by `cancel_query`: long jobs (scripts, imports) check it between
    /// steps, where no statement is waiting on `cancel`.
    pub cancelled: std::sync::atomic::AtomicBool,
    pub interrupter: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The session commits each statement: it starts as the connection's
    /// «autocommit» option says (on when missing), and follows the tab's
    /// Auto/Manual toggle after that.
    pub autocommit: std::sync::atomic::AtomicBool,
    /// The database a statement switched the session to (T-SQL `USE`…, see
    /// `QueryOutcome::database`): the tab now asks for it and keeps this
    /// session instead of opening a new one.
    pub switched_to: std::sync::Mutex<Option<String>>,
}

/// Whether a new session commits each statement: drivers with an
/// «autocommit» connection option (Oracle, Firebird) open it off when the
/// option is off, so the tab's Auto toggle has to switch it on.
fn starts_in_autocommit(cfg: &ConnectionConfig) -> bool {
    cfg.option("autocommit").is_none_or(|v| v == "true")
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<StateStore>,
    pub sessions: Arc<DashMap<String, Arc<SessionEntry>>>,
    /// Secrets typed this run for connections that don't keep theirs.
    pub typed_secrets: Arc<DashMap<String, Secrets>>,
    /// Secrets read from the keychain this run: macOS may ask the user
    /// before each read, so it's read once per connection per run.
    pub keychain_cache: Arc<DashMap<String, Secrets>>,
    /// Cloud backup (set once at startup: it holds a copy of this state).
    pub sync: Arc<std::sync::OnceLock<crate::sync::SyncManager>>,
    /// Connections' SSH tunnels.
    pub tunnels: Arc<crate::tunnels::Tunnels>,
    /// The explorer's cache (`cache.db`); unset if it couldn't be opened,
    /// and then the explorer simply waits for the server as before.
    pub cache: Arc<std::sync::OnceLock<dbine_core::ExplorerCache>>,
}

/// The app, for events sent from here; set once at startup.
static APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

pub fn set_app_handle(app: tauri::AppHandle) {
    let _ = APP.set(app);
}

pub fn meta_key(connection_id: &str, database: &str) -> String {
    format!("meta:{connection_id}:{database}")
}

pub fn driver_info(id: &str) -> CommandResult<&'static DriverInfo> {
    dbine_drivers::find(id)
        .map(|d| d.info())
        .ok_or_else(|| CommandError::BadRequest(format!("esta versión no incluye el driver '{id}'")))
}

/// Put secret values where the driver reads them: `password` in its field,
/// the rest in `options`.
pub fn apply_secrets(cfg: &mut ConnectionConfig, secrets: &Secrets) {
    for (k, v) in secrets {
        if k == "password" {
            cfg.password = Some(v.clone());
        } else {
            cfg.options.insert(k.clone(), v.clone());
        }
    }
}

/// Take secret values out of a config (they go to the keychain, not to the
/// state file). Only non-empty values are returned.
pub fn extract_secrets(cfg: &mut ConnectionConfig, info: &DriverInfo) -> Secrets {
    let mut out = Secrets::new();
    for f in info.fields.iter().filter(|f| f.secret) {
        let v = if f.key == "password" { cfg.password.take() } else { cfg.options.remove(f.key) };
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            out.insert(f.key.to_string(), v);
        }
    }
    // The SSH tunnel's password and key passphrase.
    for key in crate::tunnels::SECRETS {
        if let Some(v) = cfg.options.remove(key).filter(|v| !v.is_empty()) {
            out.insert(key.to_string(), v);
        }
    }
    out
}

impl AppState {
    pub fn new(store: StateStore) -> Self {
        Self {
            store: Arc::new(store),
            sessions: Arc::new(DashMap::new()),
            typed_secrets: Arc::new(DashMap::new()),
            keychain_cache: Arc::new(DashMap::new()),
            sync: Arc::new(std::sync::OnceLock::new()),
            tunnels: Arc::default(),
            cache: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Keep the server's answer for the explorer's next opening. A failure
    /// only costs the next opening its head start.
    pub fn cache_put<T: serde::Serialize>(&self, connection_id: &str, database: &str, kind: &str, item: &str, value: &T) {
        let Some(cache) = self.cache.get() else { return };
        match serde_json::to_string(value) {
            Ok(json) => {
                if let Err(e) = cache.put(connection_id, database, kind, item, &json) {
                    tracing::warn!("explorer cache write failed: {e}");
                }
            }
            Err(e) => tracing::warn!("explorer cache encode failed: {e}"),
        }
    }

    /// Forget a connection's cached tree (deleted, or edited).
    pub fn cache_forget(&self, connection_id: &str) {
        if let Some(cache) = self.cache.get() {
            let _ = cache.forget_connection(connection_id);
        }
    }

    /// The saved connection's config with its secrets filled in.
    pub fn resolve_config(&self, connection_id: &str) -> CommandResult<ConnectionConfig> {
        let saved = self
            .store
            .get_connection(connection_id)?
            .ok_or_else(|| CommandError::NotFound(format!("no existe la conexión '{connection_id}'")))?;
        let mut cfg = saved.config;
        if let Some(s) = self.typed_secrets.get(connection_id) {
            apply_secrets(&mut cfg, &s);
        } else if saved.save_password {
            let stored = self.keychain_secrets(connection_id, &cfg)?;
            apply_secrets(&mut cfg, &stored);
        } else if needs_password(&cfg)? {
            return Err(CommandError::PasswordRequired(None));
        }
        Ok(cfg)
    }

    /// The connection's secrets from the keychain (cached for the run). When
    /// the keychain refuses (macOS asks the user, and a development build
    /// asks again after every rebuild because its signature changes), the
    /// UI is asked for the password instead of failing.
    fn keychain_secrets(&self, connection_id: &str, cfg: &ConnectionConfig) -> CommandResult<Secrets> {
        if let Some(s) = self.keychain_cache.get(connection_id) {
            return Ok(s.clone());
        }
        match secrets::get(connection_id) {
            Ok(s) => {
                self.keychain_cache.insert(connection_id.to_string(), s.clone());
                Ok(s)
            }
            Err(e) if needs_password(cfg)? => {
                tracing::warn!(%e, "keychain read refused");
                Err(CommandError::PasswordRequired(Some(format!(
                    "No se pudo leer la contraseña guardada en el llavero del sistema ({e}). \
                     Si macOS te pide acceso, se usa la contraseña de tu usuario de Mac y conviene elegir «Permitir siempre». \
                     Mientras tanto, escribí la contraseña de la base:"
                ))))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The session under `key`, opened on `database` if missing or bound
    /// elsewhere.
    pub async fn session(&self, key: &str, connection_id: &str, database: &str) -> CommandResult<Arc<SessionEntry>> {
        if let Some(e) = self.sessions.get(key) {
            // Where the session is now: a `USE` may have moved it.
            let now = e.switched_to.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if e.connection_id == connection_id && now.as_deref().unwrap_or(e.database.as_str()) == database {
                return Ok(e.clone());
            }
        }
        let mut cfg = self.resolve_config(connection_id)?;
        self.tunnels.route(Some(connection_id), &mut cfg).await?;
        let db = (!database.is_empty()).then_some(database);
        let session = dbine_drivers::open_session(&cfg, db).await?;
        let entry = Arc::new(SessionEntry {
            connection_id: connection_id.to_string(),
            database: database.to_string(),
            interrupter: session.interrupter(),
            session: Mutex::new(session),
            cancel: Notify::new(),
            cancelled: Default::default(),
            autocommit: std::sync::atomic::AtomicBool::new(starts_in_autocommit(&cfg)),
            switched_to: Default::default(),
        });
        self.sessions.insert(key.to_string(), entry.clone());
        Ok(entry)
    }

    /// Run a structure read (objects, columns, the whole schema…) on the
    /// database's shared metadata session, within `limit`. A catalog query
    /// blocked on the server (a lock held by a long transaction or DDL) would
    /// otherwise hang forever and queue every explorer call behind it: on
    /// timeout the query is interrupted, the session dropped (the next call
    /// opens a fresh one) and the error says what happened.
    ///
    /// Waiting for the session while another read holds it (a schema compare
    /// of a big database, a dependents scan) doesn't count toward `limit` and
    /// never interrupts that read: it has its own ceiling, also `limit`, after
    /// which this call gives up alone ("busy"). A read that fails because the
    /// connection broke drops the session too.
    pub async fn meta_read<T, F>(&self, connection_id: &str, database: &str, limit: std::time::Duration, f: F) -> CommandResult<T>
    where
        F: for<'a> FnOnce(&'a mut Box<dyn Session>) -> std::pin::Pin<Box<dyn std::future::Future<Output = dbine_driver::Result<T>> + Send + 'a>>,
    {
        let key = meta_key(connection_id, database);
        let entry = self.session(&key, connection_id, database).await?;
        let on = if database.is_empty() { String::new() } else { format!(" de «{database}»") };
        let Ok(mut s) = tokio::time::timeout(limit, entry.session.lock()).await else {
            return Err(CommandError::Connect(format!(
                "la conexión sigue ocupada leyendo la estructura{on} (por ejemplo, una comparación de esquemas en curso); volvé a intentar en un momento."
            )));
        };
        match tokio::time::timeout(limit, f(&mut s)).await {
            Ok(Err(e @ (dbine_driver::Error::Connect(_) | dbine_driver::Error::Io(_)))) => {
                drop(s);
                self.sessions.remove_if(&key, |_, held| Arc::ptr_eq(held, &entry));
                Err(e.into())
            }
            Ok(r) => Ok(r?),
            Err(_) => {
                if let Some(i) = &entry.interrupter {
                    i();
                }
                entry.cancel.notify_waiters();
                drop(s);
                self.sessions.remove_if(&key, |_, e| Arc::ptr_eq(e, &entry));
                Err(CommandError::Connect(format!(
                    "el servidor no respondió en {} s leyendo la estructura{on}. Puede estar bloqueado por una transacción o un cambio de estructura en curso; se canceló la consulta, volvé a intentar.",
                    limit.as_secs(),
                )))
            }
        }
    }

    /// A session of its own for a long job (export, script, import), under
    /// `key` so `cancel_query` reaches it. `read_only` forces read-only
    /// whatever the connection says.
    pub async fn dedicated_session(
        &self,
        key: &str,
        connection_id: &str,
        database: &str,
        read_only: bool,
    ) -> CommandResult<Arc<SessionEntry>> {
        let mut cfg = self.resolve_config(connection_id)?;
        cfg.read_only |= read_only;
        self.tunnels.route(Some(connection_id), &mut cfg).await?;
        let db = (!database.is_empty()).then_some(database);
        let session = dbine_drivers::open_session(&cfg, db).await?;
        let entry = Arc::new(SessionEntry {
            connection_id: connection_id.to_string(),
            database: database.to_string(),
            interrupter: session.interrupter(),
            session: Mutex::new(session),
            cancel: Notify::new(),
            cancelled: Default::default(),
            autocommit: std::sync::atomic::AtomicBool::new(starts_in_autocommit(&cfg)),
            switched_to: Default::default(),
        });
        self.sessions.insert(key.to_string(), entry.clone());
        Ok(entry)
    }

    /// Drop every session of a connection (disconnect, edit, delete). Every
    /// window hears it (`state-changed`, kind `sessions-closed`): a tab of
    /// another window on that connection is now disconnected too.
    pub fn close_connection_sessions(&self, connection_id: &str) {
        self.sessions.retain(|_, e| {
            let keep = e.connection_id != connection_id;
            if !keep {
                e.cancel.notify_waiters();
            }
            keep
        });
        self.tunnels.close(connection_id);
        if let Some(app) = APP.get() {
            use tauri::Emitter;
            let _ = app.emit(
                "state-changed",
                serde_json::json!({ "kind": "sessions-closed", "id": null, "connection_id": connection_id, "database": null }),
            );
        }
    }
}

/// Whether to ask for the password before connecting: the driver has a
/// password field and the connection names a user.
fn needs_password(cfg: &ConnectionConfig) -> CommandResult<bool> {
    let info = driver_info(&cfg.driver)?;
    let has_field = info.fields.iter().any(|f| f.key == "password");
    Ok(has_field && cfg.username.as_deref().is_some_and(|u| !u.is_empty()))
}

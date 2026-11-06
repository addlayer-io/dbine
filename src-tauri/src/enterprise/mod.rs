//! Signed in to DBine Enterprise: the company's connections, ready to use.
//!
//! The user signs in once with the company's SSO (`signin.rs`). DBine then
//! gets the connections and profiles it may list (`sync`), keeps one managed
//! connection row per profile the user has access to (dbine_core's
//! `set_managed_connections`: never in backups, sync, MCP or the phone),
//! and opens their sessions through the gateway (`gateway.rs`), which holds
//! the servers and the accounts: DBine never sees them. A WebSocket tells it
//! what changes (`events.rs`): decisions, grants given, revoked or ending,
//! sessions draining or ended. The token lives in the system keychain.

pub mod channel;
mod events;
mod gateway;
mod signin;

use crate::error::{CommandError, CommandResult};
use chrono::{DateTime, Utc};
use dbine_core::{SavedConnection, StateStore};
use dbine_driver::ConnectionConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

pub use gateway::open_session;

fn gateway_tls() -> Arc<rustls::ClientConfig> {
    gateway::tls()
}

/// The option a managed connection carries: the profile the gateway opens.
pub const PROFILE_OPTION: &str = "dbine.gateway.profile";
/// The company connection it belongs to, and its profile's name (for the UI).
pub const CONNECTION_OPTION: &str = "dbine.enterprise.connection";
pub const PROFILE_NAME_OPTION: &str = "dbine.enterprise.profile";

/// Where the server and who signed in are kept (a machine-local setting,
/// never backed up); the token is in the keychain under [`KEYCHAIN_KEY`].
const SETTING: &str = "local.enterprise";
const KEYCHAIN_KEY: &str = "dbine-enterprise-session";

/// Renew the token when it has less than this left.
const RENEW_BEFORE_DAYS: i64 = 2;

/// What DBine Enterprise's server and gateway may answer, in Spanish like
/// every backend message: listed here so the UI's catalog translates them
/// (scripts/i18n/extract_backend.py).
#[allow(dead_code)]
const SERVER_MESSAGES: &[&str] = &[
    "esta conexión no pasa por DBine Bastion",
    "no tenés acceso a esta conexión: pedilo desde DBine Enterprise",
    "tu acceso a esta conexión terminó o fue revocado",
    "tu acceso a esta conexión terminó: lo que estaba corriendo puede terminar, pero no se aceptan sentencias nuevas",
    "tu perfil de acceso a esta conexión es de solo lectura",
    "no tenés acceso a la base de datos «{}»",
    "la sesión no existe",
    "la conexión no está disponible en DBine Bastion",
    "DBine Bastion no permite «{}»",
    "ya hay un pedido tuyo de ese perfil esperando una decisión",
    "una extensión necesita una justificación",
    "no hay un acceso a ese perfil para extender",
    "ese acceso no vence",
    "la fecha hasta la que se pide el acceso ya pasó",
    "el acceso se pide por 1 a {} horas",
    "un pedido necesita un motivo",
    "el pedido ya fue decidido",
    "el pedido no existe",
    "el perfil no existe",
    "nadie aprueba su propio pedido",
    "ya tenés {} sesiones abiertas con este perfil: cerrá alguna para abrir otra",
    "tu perfil permite sacar hasta {} filas por consulta: la exportación se detuvo",
];

/// Whether a connection is one of the company's.
pub fn is_managed(cfg: &ConnectionConfig) -> bool {
    cfg.option(PROFILE_OPTION).is_some()
}

/// The company's connections are changed only by DBine Enterprise.
pub fn refuse_if_managed(store: &StateStore, id: &str) -> CommandResult<()> {
    if store.is_managed_connection(id)? {
        return Err(CommandError::BadRequest(
            "las conexiones de DBine Enterprise las administra la empresa: no se cambian desde acá"
                .into(),
        ));
    }
    Ok(())
}

/// The managed connection's id for a profile.
pub fn connection_id(profile_id: &str) -> String {
    format!("ent-{profile_id}")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Saved {
    server: String,
    email: String,
    name: String,
    organization: String,
    /// The last event DBine has seen: the stream resumes after it.
    last_event: i64,
    /// Where DBine Bastion is, and its key, pinned at the first sync after
    /// signing in: a Bastion answering with another key is refused.
    #[serde(default)]
    bastion_url: String,
    #[serde(default)]
    bastion_key: String,
}

/// This computer's key for the end-to-end channel (private, public), kept in
/// the keychain: DBine Enterprise binds the token to its public half.
const DEVICE_KEY: &str = "dbine-enterprise-device";

#[derive(Serialize, Deserialize)]
struct DeviceKey {
    private: String,
    public: String,
}

pub(crate) fn device_key() -> CommandResult<(Vec<u8>, Vec<u8>)> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    if let Some(k) = dbine_core::secrets::get_raw(DEVICE_KEY)?
        .and_then(|j| serde_json::from_str::<DeviceKey>(&j).ok())
    {
        if let (Ok(p), Ok(q)) = (b64.decode(&k.private), b64.decode(&k.public)) {
            return Ok((p, q));
        }
    }
    let (private, public) = channel::keypair();
    let j = serde_json::to_string(&DeviceKey {
        private: b64.encode(&private),
        public: b64.encode(&public),
    })
    .map_err(|e| CommandError::Internal(e.to_string()))?;
    dbine_core::secrets::set_raw(DEVICE_KEY, &j)?;
    Ok((private, public))
}

/// What this computer calls itself, for the console's list of computers.
fn device_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|h| h.trim_end_matches(".local").to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| format!("DBine ({})", std::env::consts::OS))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Token {
    token: String,
    expires_at: DateTime<Utc>,
}

/// What DBine Enterprise says DBine may list: as the server sends it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub organization: String,
    pub settings: Value,
    pub connections: Vec<CatalogConnection>,
    pub last_event: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogConnection {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub package: String,
    /// When it may be used (null: at any time). The server's rule: DBine
    /// only shows it and offers to ask for access outside it.
    #[serde(default)]
    pub schedule: Option<serde_json::Value>,
    #[serde(default = "yes")]
    pub within_hours: bool,
    pub profiles: Vec<CatalogProfile>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub read_only: bool,
    /// `{databases, until}`; null without access.
    pub access: Option<Value>,
    pub pending_request: Option<Value>,
}

/// What the UI shows: signed in or not, the company's catalog and whether
/// DBine Enterprise answers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub signed_in: bool,
    pub server: String,
    pub email: String,
    pub name: String,
    pub organization: String,
    /// The last sync reached the server.
    pub online: bool,
    /// Why it didn't, in that case.
    pub error: Option<String>,
    pub catalog: Option<Catalog>,
}

pub struct Enterprise {
    app: AppHandle,
    store: Arc<StateStore>,
    /// Built on first use: without a sign-in, starting the app doesn't pay for it.
    http: std::sync::OnceLock<reqwest::Client>,
    saved: Mutex<Option<Saved>>,
    token: Mutex<Option<Token>>,
    status: Mutex<Status>,
    /// The events stream; replaced on sign-in, stopped on sign-out.
    events: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// A sign-in waiting in the browser; sending cancels it.
    cancel_sign_in: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    /// Signed out here: the browser still has the identity provider's session,
    /// so the next sign-in asks who it is instead of reusing it.
    signed_out: std::sync::atomic::AtomicBool,
}

static ENTERPRISE: OnceLock<Arc<Enterprise>> = OnceLock::new();

/// Starts at app start: resumes a sign-in kept from before.
pub fn init(app: AppHandle, store: Arc<StateStore>) {
    let saved: Option<Saved> = store
        .get_setting(SETTING)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok());
    let token: Option<Token> = saved.as_ref().and_then(|_| {
        dbine_core::secrets::get_raw(KEYCHAIN_KEY)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
    });
    let e = Arc::new(Enterprise {
        app,
        store,
        http: std::sync::OnceLock::new(),
        saved: Mutex::new(saved.clone()),
        token: Mutex::new(token.clone()),
        status: Mutex::new(Status::default()),
        events: Mutex::new(None),
        cancel_sign_in: Mutex::new(None),
        signed_out: std::sync::atomic::AtomicBool::new(false),
    });
    let _ = ENTERPRISE.set(e.clone());
    e.refresh_status_fields();
    if saved.is_some() && token.is_some() {
        tauri::async_runtime::spawn(async move {
            let _ = e.sync().await;
            e.start_events();
        });
    } else if e
        .store
        .list_managed_connections()
        .is_ok_and(|rows| !rows.is_empty())
    {
        // Managed rows from a session that ended without signing out.
        let _ = e.store.set_managed_connections(&[]);
    }
}

impl Enterprise {
    fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default()
        })
    }
}

pub fn get() -> CommandResult<Arc<Enterprise>> {
    ENTERPRISE
        .get()
        .cloned()
        .ok_or_else(|| CommandError::Internal("DBine Enterprise no está listo".into()))
}

/// `https://…` without a trailing slash; plain http only to this machine.
fn normalize_server(input: &str) -> CommandResult<String> {
    let s = input.trim().trim_end_matches('/');
    let with_scheme = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    let u = url::Url::parse(&with_scheme)
        .map_err(|_| CommandError::BadRequest("la dirección del servidor no es válida".into()))?;
    let local = matches!(
        u.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("[::1]")
    );
    match u.scheme() {
        "https" => {}
        "http" if local => {}
        _ => {
            return Err(CommandError::BadRequest(
                "DBine Enterprise se usa por https".into(),
            ))
        }
    }
    if u.host_str().is_none() {
        return Err(CommandError::BadRequest(
            "la dirección del servidor no es válida".into(),
        ));
    }
    Ok(with_scheme)
}

impl Enterprise {
    fn server(&self) -> Option<String> {
        self.saved
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.server.clone())
    }

    /// DBine Bastion's address and pinned key (base64).
    fn bastion(&self) -> Option<(String, String)> {
        self.saved
            .lock()
            .unwrap()
            .as_ref()
            .filter(|s| !s.bastion_url.is_empty() && !s.bastion_key.is_empty())
            .map(|s| (s.bastion_url.clone(), s.bastion_key.clone()))
    }

    fn refresh_status_fields(&self) {
        let saved = self.saved.lock().unwrap().clone();
        let signed_in = saved.is_some() && self.token.lock().unwrap().is_some();
        let mut st = self.status.lock().unwrap();
        st.signed_in = signed_in;
        if let Some(s) = saved {
            st.server = s.server;
            st.email = s.email;
            st.name = s.name;
            if !s.organization.is_empty() {
                st.organization = s.organization;
            }
        } else {
            *st = Status::default();
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn emit_status(&self) {
        let _ = self.app.emit("enterprise-status", self.status());
    }

    fn persist(&self) {
        let saved = self.saved.lock().unwrap().clone();
        let v = saved.map(|s| serde_json::to_value(s).unwrap_or(Value::Null));
        if let Err(e) = self.store.set_setting(SETTING, v.as_ref()) {
            tracing::warn!("saving the DBine Enterprise sign-in: {e}");
        }
    }

    fn keep_token(&self, t: Option<Token>) {
        match &t {
            Some(t) => {
                if let Ok(j) = serde_json::to_string(t) {
                    if let Err(e) = dbine_core::secrets::set_raw(KEYCHAIN_KEY, &j) {
                        tracing::warn!("keeping the DBine Enterprise token: {e}");
                    }
                }
            }
            None => {
                let _ = dbine_core::secrets::delete_raw(KEYCHAIN_KEY);
            }
        }
        *self.token.lock().unwrap() = t;
    }

    /// The bearer token, renewed first when it's about to run out.
    pub(crate) async fn bearer(&self) -> CommandResult<String> {
        let current = self.token.lock().unwrap().clone();
        let Some(t) = current else {
            return Err(CommandError::AuthFailed(
                "iniciá sesión en DBine Enterprise".into(),
            ));
        };
        if t.expires_at - Utc::now() > chrono::Duration::days(RENEW_BEFORE_DAYS) {
            return Ok(t.token);
        }
        let server = self.server().unwrap_or_default();
        let r = self
            .http()
            .post(format!("{server}/api/v1/auth/dbine/refresh"))
            .bearer_auth(&t.token)
            .send()
            .await;
        match r {
            Ok(r) if r.status().is_success() => {
                let fresh: Token = r
                    .json()
                    .await
                    .map_err(|e| CommandError::Internal(e.to_string()))?;
                let token = fresh.token.clone();
                self.keep_token(Some(fresh));
                Ok(token)
            }
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {
                self.session_lost().await;
                Err(CommandError::AuthFailed(
                    "la sesión de DBine Enterprise terminó: volvé a iniciarla".into(),
                ))
            }
            // Unreachable for now: the old token still works a while.
            _ => Ok(t.token),
        }
    }

    /// The server refused the token: signed out, the company's connections
    /// gone.
    async fn session_lost(&self) {
        self.keep_token(None);
        let _ = self.store.set_managed_connections(&[]);
        gateway::forget_drivers();
        self.refresh_status_fields();
        {
            let mut st = self.status.lock().unwrap();
            st.catalog = None;
            st.online = false;
            st.error = Some("la sesión de DBine Enterprise terminó: volvé a iniciarla".into());
        }
        let _ = self.app.emit(
            "enterprise-event",
            serde_json::json!({ "kind": "session.lost" }),
        );
        self.emit_status();
    }

    /// A call to DBine Enterprise's API for DBine, as JSON.
    pub(crate) async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> CommandResult<Value> {
        let server = self
            .server()
            .ok_or_else(|| CommandError::AuthFailed("iniciá sesión en DBine Enterprise".into()))?;
        let token = self.bearer().await?;
        let mut req = self
            .http()
            .request(method, format!("{server}/api/v1{path}"))
            .bearer_auth(token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let r = req
            .send()
            .await
            .map_err(|e| CommandError::Connect(format!("DBine Enterprise no responde ({e})")))?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.session_lost().await;
            return Err(CommandError::AuthFailed(
                "la sesión de DBine Enterprise terminó: volvé a iniciarla".into(),
            ));
        }
        if !status.is_success() {
            let msg = v["message"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("DBine Enterprise respondió {status}"));
            return Err(match status {
                reqwest::StatusCode::NOT_FOUND => CommandError::NotFound(msg),
                reqwest::StatusCode::FORBIDDEN => CommandError::AuthFailed(msg),
                _ => CommandError::BadRequest(msg),
            });
        }
        Ok(v)
    }

    /// Gets the catalog and makes the managed connections match it.
    pub async fn sync(&self) -> CommandResult<Status> {
        let res = self.api(reqwest::Method::GET, "/desktop/sync", None).await;
        let v = match res {
            Ok(v) => v,
            Err(e) => {
                {
                    let mut st = self.status.lock().unwrap();
                    st.online = false;
                    st.error = Some(e.to_string());
                }
                self.emit_status();
                return Err(e);
            }
        };
        let catalog: Catalog = serde_json::from_value(serde_json::json!({
            "organization": v["organization"],
            "settings": v["settings"],
            "connections": v["connections"],
            "last_event": v["last_event"],
        }))
        .map_err(|e| {
            CommandError::Internal(format!("respuesta inesperada de DBine Enterprise: {e}"))
        })?;
        // Bastion's key: pinned the first time, the same every time after.
        let url = v["bastion"]["url"].as_str().unwrap_or_default().to_string();
        let key = v["bastion"]["key"].as_str().unwrap_or_default().to_string();
        let changed = {
            let mut saved = self.saved.lock().unwrap();
            match saved.as_mut() {
                Some(s) if !s.bastion_key.is_empty() && !key.is_empty() && s.bastion_key != key => {
                    true
                }
                Some(s) => {
                    if s.bastion_key.is_empty() {
                        s.bastion_key = key;
                    }
                    s.bastion_url = url;
                    false
                }
                None => false,
            }
        };
        if changed {
            let e = CommandError::AuthFailed(
                "DBine Bastion se presentó con otra clave: si la empresa la cambió, volvé a iniciar sesión".into(),
            );
            {
                let mut st = self.status.lock().unwrap();
                st.online = false;
                st.error = Some(e.to_string());
            }
            self.emit_status();
            return Err(e);
        }
        let rows = managed_rows(&catalog);
        self.store.set_managed_connections(&rows)?;
        // Profiles no longer granted: their sessions end here too.
        if let Some(state) = crate::state::app_state() {
            let keep: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
            let gone: Vec<String> = state
                .sessions
                .iter()
                .filter(|e| e.connection_id.starts_with("ent-") && !keep.contains(&e.connection_id))
                .map(|e| e.connection_id.clone())
                .collect();
            for id in gone {
                state.close_connection_sessions(&id);
            }
        }
        if let Some(s) = self.saved.lock().unwrap().as_mut() {
            s.organization = catalog.organization.clone();
            s.email = v["user"]["email"].as_str().unwrap_or(&s.email).to_string();
            s.name = v["user"]["name"].as_str().unwrap_or(&s.name).to_string();
        }
        self.persist();
        self.refresh_status_fields();
        {
            let mut st = self.status.lock().unwrap();
            st.online = true;
            st.error = None;
            st.catalog = Some(catalog);
        }
        self.emit_status();
        Ok(self.status())
    }

    /// Whether the company lets the AI assistant, MCP or the explorer cache
    /// use its connections (`allow_ai`, `allow_mcp`, `explorer_cache`).
    pub fn allows(&self, what: &str) -> bool {
        self.status
            .lock()
            .unwrap()
            .catalog
            .as_ref()
            .and_then(|c| c.settings[what].as_bool())
            .unwrap_or(false)
    }

    /// The driver crate a managed connection's profile goes through.
    pub(crate) fn package_of(&self, profile_id: &str) -> Option<String> {
        self.status
            .lock()
            .unwrap()
            .catalog
            .as_ref()?
            .connections
            .iter()
            .find(|c| c.profiles.iter().any(|p| p.id == profile_id))
            .map(|c| c.package.clone())
    }

    pub async fn sign_out(&self) -> CommandResult<()> {
        self.signed_out
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.events.lock().unwrap().take() {
            h.abort();
        }
        let token = self.token.lock().unwrap().clone();
        if let (Some(server), Some(t)) = (self.server(), token) {
            let _ = self
                .http()
                .post(format!("{server}/api/v1/auth/dbine/logout"))
                .bearer_auth(&t.token)
                .send()
                .await;
        }
        if let Some(state) = crate::state::app_state() {
            let open: Vec<String> = state
                .sessions
                .iter()
                .filter(|e| e.connection_id.starts_with("ent-"))
                .map(|e| e.connection_id.clone())
                .collect();
            for id in open {
                state.close_connection_sessions(&id);
                state.cache_forget(&id);
            }
        }
        for c in self.store.list_managed_connections()? {
            if let Some(state) = crate::state::app_state() {
                state.cache_forget(&c.id);
            }
        }
        self.store.set_managed_connections(&[])?;
        gateway::forget_drivers();
        self.keep_token(None);
        *self.saved.lock().unwrap() = None;
        self.persist();
        self.refresh_status_fields();
        self.emit_status();
        Ok(())
    }

    fn start_events(self: &Arc<Self>) {
        let me = self.clone();
        let h = tauri::async_runtime::spawn(async move { events::run(me).await });
        if let Some(old) = self.events.lock().unwrap().replace(h) {
            old.abort();
        }
    }

    fn last_event(&self) -> i64 {
        self.saved
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.last_event)
            .unwrap_or(0)
    }

    fn set_last_event(&self, id: i64) {
        if let Some(s) = self.saved.lock().unwrap().as_mut() {
            s.last_event = id;
        }
        self.persist();
    }
}

/// One managed connection per profile the user may use now.
fn managed_rows(c: &Catalog) -> Vec<SavedConnection> {
    let mut out = Vec::new();
    for conn in &c.connections {
        let granted: Vec<&CatalogProfile> = conn
            .profiles
            .iter()
            .filter(|p| p.access.is_some())
            .collect();
        for p in &granted {
            let name = if granted.len() > 1 {
                format!("{} · {}", conn.name, p.name)
            } else {
                conn.name.clone()
            };
            let config = ConnectionConfig {
                driver: conn.driver.clone(),
                read_only: p.read_only,
                options: [
                    (PROFILE_OPTION.to_string(), p.id.clone()),
                    (CONNECTION_OPTION.to_string(), conn.id.clone()),
                    (PROFILE_NAME_OPTION.to_string(), p.name.clone()),
                ]
                .into(),
                ..Default::default()
            };
            out.push(SavedConnection {
                id: connection_id(&p.id),
                name,
                color: None,
                config,
                save_password: false,
                folder_id: None,
                tags: Vec::new(),
                mcp_level: Some("disabled".into()),
                updated_at: String::new(),
            });
        }
    }
    out
}

// ---- commands ----

#[tauri::command]
pub async fn enterprise_status() -> CommandResult<Status> {
    Ok(get()?.status())
}

#[derive(Deserialize)]
pub struct SignInArgs {
    pub server: String,
    /// The page the browser shows once signed in, in the UI's language.
    pub done_page: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn enterprise_sign_in(args: SignInArgs) -> CommandResult<Status> {
    let e = get()?;
    let server = normalize_server(&args.server)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    if let Some(old) = e.cancel_sign_in.lock().unwrap().replace(tx) {
        let _ = old.send(());
    }
    let (_, device_public) = device_key()?;
    let name = device_name();
    let fresh = e.signed_out.load(std::sync::atomic::Ordering::Relaxed);
    let signed = tokio::select! {
        r = signin::run(&e.app, e.http(), &server, &args.done_page, &device_public, &name, fresh) => r?,
        _ = rx => return Err(CommandError::Cancelled),
    };
    e.signed_out
        .store(false, std::sync::atomic::Ordering::Relaxed);
    // Another account's managed connections go first.
    if e.saved
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|s| s.server != server || s.email != signed.email)
    {
        let _ = e.store.set_managed_connections(&[]);
        gateway::forget_drivers();
    }
    *e.saved.lock().unwrap() = Some(Saved {
        server,
        email: signed.email,
        name: signed.name,
        organization: String::new(),
        last_event: 0,
        bastion_url: String::new(),
        bastion_key: String::new(),
    });
    e.keep_token(Some(Token {
        token: signed.token,
        expires_at: signed.expires_at,
    }));
    e.persist();
    let status = e.sync().await?;
    let last = status.catalog.as_ref().map(|c| c.last_event).unwrap_or(0);
    e.set_last_event(last);
    e.start_events();
    Ok(status)
}

#[tauri::command]
pub async fn enterprise_cancel_sign_in() -> CommandResult<()> {
    if let Some(tx) = get()?.cancel_sign_in.lock().unwrap().take() {
        let _ = tx.send(());
    }
    Ok(())
}

#[tauri::command]
pub async fn enterprise_sign_out() -> CommandResult<()> {
    get()?.sign_out().await
}

#[tauri::command]
pub async fn enterprise_sync() -> CommandResult<Status> {
    get()?.sync().await
}

#[derive(Deserialize)]
pub struct RequestArgs {
    pub profile_id: String,
    pub databases: Option<Vec<String>>,
    pub hours: Option<u32>,
    pub until: Option<String>,
    pub reason: String,
    #[serde(default)]
    pub outside_hours: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn enterprise_request(args: RequestArgs) -> CommandResult<String> {
    let e = get()?;
    let v = e
        .api(
            reqwest::Method::POST,
            "/desktop/requests",
            Some(&serde_json::json!({
                "profile_id": args.profile_id, "databases": args.databases,
                "hours": args.hours, "until": args.until, "reason": args.reason,
                "outside_hours": args.outside_hours,
            })),
        )
        .await?;
    let _ = e.sync().await;
    Ok(v["id"].as_str().unwrap_or_default().to_string())
}

#[derive(Deserialize)]
pub struct RunningArg {
    pub statement: String,
    pub seconds: u64,
}

#[derive(Deserialize)]
pub struct ExtendArgs {
    pub profile_id: String,
    pub hours: Option<u32>,
    pub until: Option<String>,
    pub reason: String,
    #[serde(default)]
    pub running: Vec<RunningArg>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn enterprise_extend(args: ExtendArgs) -> CommandResult<String> {
    let e = get()?;
    let running: Vec<Value> = args
        .running
        .iter()
        .map(|r| {
            // Long scripts are cut: the approver needs to recognize it.
            let statement: String = r.statement.chars().take(4000).collect();
            serde_json::json!({ "statement": statement, "seconds": r.seconds })
        })
        .collect();
    let v = e
        .api(
            reqwest::Method::POST,
            "/desktop/extensions",
            Some(&serde_json::json!({
                "profile_id": args.profile_id, "hours": args.hours, "until": args.until,
                "reason": args.reason, "running": running,
            })),
        )
        .await?;
    let _ = e.sync().await;
    Ok(v["id"].as_str().unwrap_or_default().to_string())
}

#[derive(Deserialize)]
pub struct CancelArgs {
    pub request_id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn enterprise_cancel_request(args: CancelArgs) -> CommandResult<()> {
    let e = get()?;
    let path = format!("/desktop/requests/{}/cancel", urlencoding(&args.request_id));
    e.api(reqwest::Method::POST, &path, None).await?;
    let _ = e.sync().await;
    Ok(())
}

#[tauri::command]
pub async fn enterprise_requests() -> CommandResult<Value> {
    get()?
        .api(reqwest::Method::GET, "/desktop/requests", None)
        .await
}

#[derive(Deserialize)]
pub struct NotifyArgs {
    pub title: String,
    pub body: String,
}

/// A system notification, worded by the UI in the user's language: shown
/// when DBine isn't the window in front.
#[tauri::command(rename_all = "camelCase")]
pub async fn enterprise_notify(app: AppHandle, args: NotifyArgs) -> CommandResult<()> {
    use tauri_plugin_notification::NotificationExt;
    app.notification()
        .builder()
        .title(args.title)
        .body(args.body)
        .show()
        .map_err(|e| CommandError::Internal(e.to_string()))
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_https_except_this_machine() {
        assert_eq!(
            normalize_server("dbine.acme.com/").unwrap(),
            "https://dbine.acme.com"
        );
        assert_eq!(
            normalize_server(" https://dbine.acme.com:8443 ").unwrap(),
            "https://dbine.acme.com:8443"
        );
        assert_eq!(
            normalize_server("http://localhost:8080").unwrap(),
            "http://localhost:8080"
        );
        assert!(normalize_server("http://dbine.acme.com").is_err());
        assert!(normalize_server("ftp://x").is_err());
        assert!(normalize_server("").is_err());
    }

    #[test]
    fn one_managed_connection_per_granted_profile() {
        let cat: Catalog = serde_json::from_value(serde_json::json!({
            "organization": "ACME", "settings": {}, "last_event": 3,
            "connections": [
                {"id": "c1", "name": "Ventas", "driver": "postgres", "package": "postgres", "profiles": [
                    {"id": "p1", "name": "Reportes", "read_only": true, "access": {"databases": null, "until": null}, "pending_request": null},
                    {"id": "p2", "name": "Desarrollo", "access": {"databases": null, "until": null}, "pending_request": null},
                    {"id": "p3", "name": "DBA", "access": null, "pending_request": null}
                ]},
                {"id": "c2", "name": "ERP", "driver": "sqlserver", "package": "sqlserver", "profiles": [
                    {"id": "p4", "name": "Lectura", "access": {"databases": ["ERP"], "until": "2026-11-05T10:00:00Z"}, "pending_request": null}
                ]},
                {"id": "c3", "name": "HR", "driver": "mysql", "package": "mysql", "profiles": [
                    {"id": "p5", "name": "Lectura", "access": null, "pending_request": {"id": "r1"}}
                ]}
            ]
        }))
        .unwrap();
        let rows = managed_rows(&cat);
        let names: Vec<_> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("ent-p1", "Ventas · Reportes"),
                ("ent-p2", "Ventas · Desarrollo"),
                ("ent-p4", "ERP")
            ]
        );
        let r = &rows[0];
        assert!(is_managed(&r.config));
        assert_eq!(r.config.option(PROFILE_OPTION), Some("p1"));
        assert_eq!(r.config.driver, "postgres");
        assert!(r.config.read_only);
        // Nothing about the server.
        assert!(
            r.config.host.is_empty() && r.config.username.is_none() && r.config.password.is_none()
        );
        assert_eq!(r.mcp_level.as_deref(), Some("disabled"));
    }
}

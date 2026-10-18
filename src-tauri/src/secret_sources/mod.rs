//! Secret fields whose value comes from a command or a secret manager
//! (docs/secret-sources.md). The definitions are in the connection's
//! options (crates/dbine-core/src/secret_sources.rs); here they are
//! resolved, right before a session opens and before the SSH tunnel and the
//! driver get the config, so every engine gets them the same way.
//!
//! - The value lives in memory only: in the config handed to the driver
//!   and, for a minute, in a cache so the sessions a connect opens together
//!   (explorer, editor tabs) don't each run the command again. Disconnecting,
//!   editing or deleting the connection drops it.
//! - Nothing here logs a value, and errors carry stderr or the provider's
//!   message, never stdout or the secret.
//! - A definition that wasn't confirmed on this computer doesn't run: the
//!   error carries what to show (`secret_source_approval`) and the UI asks.

#[path = "../../../crates/drivers/postgres/src/cloud_auth.rs"]
#[allow(dead_code, unused_imports)]
pub(crate) mod cloud_auth;

mod aws;
mod azure;
pub mod exec;
mod gcp;
mod onepassword;
mod vault;

use crate::error::{CommandError, CommandResult};
use crate::state::{driver_info, field_applies, AppState};
use dbine_core::secret_sources::{self as defs, SecretSource, SecretSources, TOKEN_PREFIX};
use dbine_core::secrets::{self, Secrets};
use dbine_core::StateStore;
use dbine_driver::{ConnectionConfig, DriverInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tauri::State;

/// A resolved secret; wiped when dropped.
pub struct Resolved {
    pub value: String,
    /// The `username` of the same secret (AWS, when asked to fill it).
    pub username: Option<String>,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved").field("chars", &self.value.chars().count()).finish_non_exhaustive()
    }
}

impl Drop for Resolved {
    fn drop(&mut self) {
        defs::wipe(&mut self.value);
        if let Some(u) = &mut self.username {
            defs::wipe(u);
        }
    }
}

/// Where definitions are being used from.
#[derive(Clone, Copy)]
pub enum Origin<'a> {
    /// A saved connection connecting: each definition must be confirmed.
    Saved { id: &'a str, name: &'a str },
    /// The connection form ("Probar"): what the user typed here counts as
    /// confirmed, a definition that came from outside doesn't.
    Form { id: Option<&'a str>, name: &'a str },
}

impl Origin<'_> {
    fn id(&self) -> &str {
        match self {
            Origin::Saved { id, .. } => id,
            Origin::Form { id, .. } => id.unwrap_or(""),
        }
    }
    fn name(&self) -> &str {
        match self {
            Origin::Saved { name, .. } | Origin::Form { name, .. } => name,
        }
    }
}

/// What the confirmation shows: the connection, the field and the exact
/// definition. Only the UI gets it (it's in the error's `approval`, which
/// MCP and the phone never see: their errors are the message alone).
#[derive(Debug, Clone, Serialize)]
pub struct Approval {
    pub connection_id: Option<String>,
    pub connection_name: String,
    pub field: String,
    pub field_label: String,
    pub source: SecretSource,
}

/// How long a resolved value is reused by sessions opening together.
const CACHE_TTL: Duration = Duration::from_secs(60);

type Cache = HashMap<String, (Instant, Arc<Resolved>)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

fn cache_key(connection_id: &str, digest: &str) -> String {
    format!("{connection_id}\u{0}{digest}")
}

/// Drop a connection's resolved values (disconnect, edit, delete).
pub fn forget_cached(connection_id: &str) {
    let prefix = format!("{connection_id}\u{0}");
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| !k.starts_with(&prefix));
}

fn cached(key: &str) -> Option<Arc<Resolved>> {
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    c.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
    c.get(key).map(|(_, r)| r.clone())
}

/// The field's name for messages: the driver's label, or the tunnel's.
pub fn field_label(info: Option<&DriverInfo>, field: &str) -> String {
    match field {
        "ssh.password" => "Contraseña del túnel SSH".into(),
        "ssh.passphrase" => "Frase de paso de la clave SSH".into(),
        _ => info.and_then(|i| i.fields.iter().find(|f| f.key == field)).map(|f| f.label.to_string()).unwrap_or_else(|| field.to_string()),
    }
}

/// Whether the field is in use with these settings (a source for a field
/// another auth method hides, or for a tunnel that's off, is left alone).
pub fn applies(info: Option<&DriverInfo>, cfg: &ConnectionConfig, field: &str) -> bool {
    let ssh_auth = || cfg.option("ssh.auth").unwrap_or("password");
    match field {
        "ssh.password" => crate::tunnels::enabled(cfg) && ssh_auth() == "password",
        "ssh.passphrase" => crate::tunnels::enabled(cfg) && ssh_auth() == "key",
        _ => info.is_some_and(|i| i.fields.iter().any(|f| f.key == field && f.secret) && field_applies(&i.fields, cfg, field)),
    }
}

/// The provider's name, for messages.
fn provider(src: &SecretSource) -> &'static str {
    match src {
        SecretSource::Command { .. } => "un comando",
        SecretSource::OnePassword { .. } => "1Password",
        SecretSource::Vault { .. } => "HashiCorp Vault",
        SecretSource::Aws { .. } => "AWS Secrets Manager",
        SecretSource::Azure { .. } => "Azure Key Vault",
        SecretSource::Gcp { .. } => "Google Secret Manager",
    }
}

fn command_failure(f: exec::Failure) -> String {
    use exec::Failure::*;
    match f {
        Spawn(e) => format!("no se pudo ejecutar el comando: {e}"),
        Timeout(d) => format!("el comando no terminó en {} s y se detuvo", d.as_secs()),
        Exit { code: Some(c), stderr } if stderr.is_empty() => format!("el comando terminó con el código {c}, sin mensaje de error"),
        Exit { code: Some(c), stderr } => format!("el comando terminó con el código {c}: {stderr}"),
        Exit { code: None, stderr } if stderr.is_empty() => "el comando terminó por una señal".into(),
        Exit { code: None, stderr } => format!("el comando terminó por una señal: {stderr}"),
        TooLong => "el comando devolvió más de 64 KB".into(),
        NotText => "la salida del comando no es texto".into(),
    }
}

/// Ask the provider. `token`: the field's kept Vault token, if any.
pub async fn fetch(src: &SecretSource, token: Option<&str>) -> Result<Resolved, String> {
    let mut raw = match src {
        SecretSource::Command { command, .. } => exec::run(exec::shell_command(command), exec::COMMAND_LIMIT).await.map(exec::trim_newline).map_err(command_failure)?,
        SecretSource::OnePassword { reference, account, .. } => onepassword::read(reference, account).await?,
        SecretSource::Vault { address, path, key, namespace, kv_version, ca_file } => {
            let req = vault::Request { address, path, key, namespace, kv_version: *kv_version, ca_file };
            vault::read(&req, token).await?
        }
        SecretSource::Aws { secret_id, region, profile, .. } => aws::get_secret(secret_id, region, profile).await?,
        SecretSource::Azure { vault_url, name, version, .. } => azure::get_secret(vault_url, name, version).await?,
        SecretSource::Gcp { project, secret, version, .. } => gcp::get_secret(project, secret, version).await?,
    };
    let picked = defs::pick_json(&raw, src.json_key()).map_err(|e| e.to_string());
    let username = match src {
        SecretSource::Aws { fill_username: true, .. } => Some(defs::pick_json(&raw, "username").map_err(|e| e.to_string())),
        _ => None,
    };
    defs::wipe(&mut raw);
    let value = picked?;
    let username = username.transpose()?;
    if value.is_empty() {
        return Err(match src {
            SecretSource::Command { .. } => "el comando no devolvió nada".into(),
            _ => "el valor está vacío".into(),
        });
    }
    Ok(Resolved { value, username })
}

/// Whether `src` may run for `field` from `origin`.
fn trusted(store: &StateStore, origin: Origin<'_>, field: &str, src: &SecretSource) -> bool {
    if defs::is_approved(&defs::digest(origin.id(), field, src)) {
        return true;
    }
    match origin {
        Origin::Saved { .. } => false,
        Origin::Form { .. } => !defs::from_outside(&store.list_connections().unwrap_or_default(), field, src),
    }
}

fn check_trust(store: &StateStore, origin: Origin<'_>, info: Option<&DriverInfo>, field: &str, src: &SecretSource) -> CommandResult<()> {
    if trusted(store, origin, field, src) {
        return Ok(());
    }
    Err(CommandError::SecretSourceApproval(Box::new(Approval {
        connection_id: Some(origin.id().to_string()).filter(|i| !i.is_empty()),
        connection_name: origin.name().to_string(),
        field: field.to_string(),
        field_label: field_label(info, field),
        source: src.clone(),
    })))
}

/// The field's kept Vault token (typed in the form, kept in the keychain).
pub fn token_key(field: &str) -> String {
    format!("{TOKEN_PREFIX}{field}")
}

/// Resolve the sources of the fields in use and put the values in `cfg`
/// (`password`, or the option of the field). `tokens`: the connection's
/// kept Vault tokens.
pub async fn fill(store: &StateStore, cfg: &mut ConnectionConfig, sources: &SecretSources, tokens: &Secrets, origin: Origin<'_>) -> CommandResult<()> {
    if sources.is_empty() {
        return Ok(());
    }
    let info = driver_info(&cfg.driver).ok();
    for (field, src) in sources {
        if !applies(info, cfg, field) {
            continue;
        }
        let label = field_label(info, field);
        src.validate().map_err(|e| CommandError::SecretSource(format!("el origen de «{label}» no es válido: {e}")))?;
        check_trust(store, origin, info, field, src)?;
        let digest = defs::digest(origin.id(), field, src);
        let key = cache_key(origin.id(), &digest);
        let saved = matches!(origin, Origin::Saved { .. });
        let resolved = match cached(&key).filter(|_| saved) {
            Some(r) => r,
            None => {
                let r = Arc::new(fetch(src, tokens.get(&token_key(field)).map(String::as_str)).await.map_err(|m| CommandError::SecretSource(format!("no se pudo obtener «{label}» de {}: {m}", provider(src))))?);
                if saved {
                    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), r.clone()));
                }
                r
            }
        };
        if field == "password" {
            cfg.password = Some(resolved.value.clone());
            if let Some(u) = &resolved.username {
                cfg.username = Some(u.clone());
            }
        } else {
            cfg.options.insert(field.clone(), resolved.value.clone());
        }
    }
    Ok(())
}

/// The Vault tokens among a connection's stored secrets.
pub fn tokens_of(stored: &Secrets) -> Secrets {
    stored.iter().filter(|(k, _)| k.starts_with(TOKEN_PREFIX)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Take the tokens typed in a form out of its options.
pub fn take_tokens(cfg: &mut ConnectionConfig) -> Secrets {
    let keys: Vec<String> = cfg.options.keys().filter(|k| k.starts_with(TOKEN_PREFIX)).cloned().collect();
    keys.into_iter().filter_map(|k| cfg.options.remove(&k).filter(|v| !v.is_empty()).map(|v| (k, v))).collect()
}

// ---- commands ---------------------------------------------------------------

#[derive(Deserialize)]
pub struct TestSecretSourceArgs {
    /// The connection being edited, if it's saved.
    pub connection_id: Option<String>,
    pub field: String,
    pub source: SecretSource,
    /// A Vault token typed in the form (else the kept one).
    pub token: Option<String>,
    /// The form's engine, for the field's name in messages.
    #[serde(default)]
    pub driver: Option<String>,
}

#[derive(Serialize)]
pub struct TestSecretSourceResult {
    /// The value's length: all the UI ever learns about it.
    pub chars: usize,
    /// The secret also had a `username` (AWS, "fill the user").
    pub username: bool,
}

/// "Probar" next to a field: resolve the form's definition and say how long
/// the value is.
#[tauri::command(rename_all = "camelCase")]
pub async fn test_secret_source(state: State<'_, AppState>, args: TestSecretSourceArgs) -> CommandResult<TestSecretSourceResult> {
    args.source.validate().map_err(|e| CommandError::BadRequest(e.to_string()))?;
    let id = args.connection_id.as_deref().filter(|i| !i.is_empty());
    let saved = match id {
        Some(i) => state.store.get_connection(i)?,
        None => None,
    };
    let name = saved.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    let driver = args.driver.clone().or_else(|| saved.as_ref().map(|c| c.config.driver.clone()));
    let info = driver.as_deref().and_then(|d| driver_info(d).ok());
    check_trust(&state.store, Origin::Form { id, name: &name }, info, &args.field, &args.source)?;
    let mut token = args.token.filter(|t| !t.is_empty());
    if token.is_none() && matches!(args.source, SecretSource::Vault { .. }) {
        if let Some(i) = id {
            token = secrets::get(i).ok().and_then(|mut s| s.remove(&token_key(&args.field)));
        }
    }
    let r = fetch(&args.source, token.as_deref()).await;
    if let Some(t) = &mut token {
        defs::wipe(t);
    }
    let r = r.map_err(|m| CommandError::SecretSource(format!("no se pudo obtener «{}» de {}: {m}", field_label(info, &args.field), provider(&args.source))))?;
    Ok(TestSecretSourceResult { chars: r.value.chars().count(), username: r.username.is_some() })
}

#[derive(Deserialize)]
pub struct ApproveSecretSourceArgs {
    pub connection_id: Option<String>,
    pub field: String,
    /// The definition the dialog showed: the confirmation is bound to it.
    pub source: SecretSource,
}

/// The user confirmed a definition in the dialog that showed it.
#[tauri::command(rename_all = "camelCase")]
pub async fn approve_secret_source(args: ApproveSecretSourceArgs) -> CommandResult<()> {
    args.source.validate().map_err(|e| CommandError::BadRequest(e.to_string()))?;
    let id = args.connection_id.as_deref().unwrap_or("");
    defs::approve(&defs::digest(id, &args.field, &args.source))?;
    forget_cached(id);
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh folder under the system's temp dir.
    pub fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dbine-test-{name}-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cmd(c: &str) -> SecretSource {
        SecretSource::Command { command: c.into(), json_key: String::new() }
    }

    #[test]
    fn which_fields_apply() {
        let info = driver_info("postgres").ok();
        let mut cfg = ConnectionConfig { driver: "postgres".into(), ..Default::default() };
        assert!(applies(info, &cfg, "password"));
        assert!(!applies(info, &cfg, "ssh.password"), "no tunnel");
        cfg.options.insert("ssh.enabled".into(), "true".into());
        assert!(applies(info, &cfg, "ssh.password"));
        assert!(!applies(info, &cfg, "ssh.passphrase"));
        cfg.options.insert("ssh.auth".into(), "key".into());
        assert!(applies(info, &cfg, "ssh.passphrase"));
        assert!(!applies(info, &cfg, "host"), "not a secret field");
        assert!(!applies(info, &cfg, "nope"));
        // A cloud sign-in hides the password.
        cfg.options.insert("auth".into(), "aws_iam".into());
        assert!(!applies(info, &cfg, "password"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn json_keys_and_usernames() {
        let r = fetch(&SecretSource::Command { command: r#"printf '{"username":"app","password":"pw"}'"#.into(), json_key: "password".into() }, None).await.unwrap();
        assert_eq!((r.value.as_str(), r.username.as_deref()), ("pw", None));
        let e = fetch(&SecretSource::Command { command: "printf plain".into(), json_key: "password".into() }, None).await.unwrap_err();
        assert!(e.contains("no es un objeto JSON") && !e.contains("plain"), "{e}");
        let e = fetch(&cmd("true"), None).await.unwrap_err();
        assert_eq!(e, "el comando no devolvió nada");
        let e = fetch(&cmd("echo leaked-stdout; echo 'denied' >&2; exit 2"), None).await.unwrap_err();
        assert_eq!(e, "el comando terminó con el código 2: denied");
    }

    /// Saved connections need a confirmation; the form trusts what was typed
    /// in it but not a copy of something that came from outside.
    #[cfg(unix)]
    #[tokio::test]
    async fn confirmations_gate_every_run() {
        let dir = temp_dir("approvals");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let marker = dir.join("ran");
        let src = cmd(&format!("touch '{}'; printf pw", marker.display()));
        let mut cfg = ConnectionConfig { driver: "postgres".into(), host: "h".into(), ..Default::default() };
        let sources: SecretSources = [("password".to_string(), src.clone())].into();
        defs::write(&mut cfg, &sources);
        let id = format!("imported-{}", uuid::Uuid::new_v4());
        store
            .save_connection(&dbine_core::SavedConnection {
                id: id.clone(),
                name: "Imported".into(),
                color: None,
                config: cfg.clone(),
                save_password: true,
                folder_id: None,
                tags: vec![],
                mcp_level: None,
                updated_at: String::new(),
            })
            .unwrap();
        let mut run_cfg = cfg.clone();
        defs::take(&mut run_cfg).unwrap();
        // Connecting: refused, with what to show.
        let e = fill(&store, &mut run_cfg, &sources, &Secrets::new(), Origin::Saved { id: &id, name: "Imported" }).await.unwrap_err();
        let CommandError::SecretSourceApproval(a) = &e else { panic!("{e:?}") };
        assert_eq!((a.connection_name.as_str(), a.field.as_str(), &a.source), ("Imported", "password", &src));
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["kind"], "secret_source_approval");
        assert!(!json["message"].as_str().unwrap().contains("touch"), "the message (what MCP sees) has no command text");
        assert!(!marker.exists(), "it didn't run");
        // The form editing it, unchanged: refused too.
        assert!(fill(&store, &mut run_cfg, &sources, &Secrets::new(), Origin::Form { id: Some(&id), name: "Imported" }).await.is_err());
        // A copy in a new form: refused (same text as the unconfirmed one).
        assert!(fill(&store, &mut run_cfg, &sources, &Secrets::new(), Origin::Form { id: None, name: "" }).await.is_err());
        assert!(!marker.exists());
        // Something typed in the form: runs.
        let typed: SecretSources = [("password".to_string(), cmd("printf typed"))].into();
        fill(&store, &mut run_cfg, &typed, &Secrets::new(), Origin::Form { id: Some(&id), name: "Imported" }).await.unwrap();
        assert_eq!(run_cfg.password.as_deref(), Some("typed"));
        // Confirmed: runs, and the value goes where the driver reads it.
        defs::approve(&defs::digest(&id, "password", &src)).unwrap();
        fill(&store, &mut run_cfg, &sources, &Secrets::new(), Origin::Saved { id: &id, name: "Imported" }).await.unwrap();
        assert!(marker.exists());
        assert_eq!(run_cfg.password.as_deref(), Some("pw"));
        // Cached for the burst: a second session doesn't run it again.
        std::fs::remove_file(&marker).unwrap();
        fill(&store, &mut run_cfg, &sources, &Secrets::new(), Origin::Saved { id: &id, name: "Imported" }).await.unwrap();
        assert!(!marker.exists(), "reused");
        forget_cached(&id);
        fill(&store, &mut run_cfg, &sources, &Secrets::new(), Origin::Saved { id: &id, name: "Imported" }).await.unwrap();
        assert!(marker.exists(), "after a disconnect it runs again");
        // The text changes: asks again.
        let changed: SecretSources = [("password".to_string(), cmd(&format!("touch '{}'; printf pw2", marker.display())))].into();
        assert!(matches!(fill(&store, &mut run_cfg, &changed, &Secrets::new(), Origin::Saved { id: &id, name: "Imported" }).await, Err(CommandError::SecretSourceApproval(_))));
        // The state never got a value.
        let snap = serde_json::to_string(&store.snapshot().unwrap()).unwrap();
        assert!(!snap.contains("\"pw\"") && !snap.contains("typed\""));
        defs::forget(&[defs::digest(&id, "password", &src)]).unwrap();
        forget_cached(&id);
    }

    /// End to end against `dbine-test-postgres` (port 25010, password `pw`):
    /// the password from a command and from Vault (`dbine-test-vault`,
    /// `secret/dbine/pg` → `password`), then a real session.
    /// `DBINE_TEST_PG_SECRETS=1 DBINE_TEST_VAULT=http://127.0.0.1:28200 cargo test -p dbine secret_sources::tests::live_postgres -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_postgres() {
        if std::env::var("DBINE_TEST_PG_SECRETS").is_err() {
            return;
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = temp_dir("e2e");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let base = ConnectionConfig { driver: "postgres".into(), host: "127.0.0.1".into(), port: 25010, database: "postgres".into(), username: Some("postgres".into()), ..Default::default() };
        let mut tries: Vec<(&str, SecretSource, Secrets)> = vec![("command", cmd("printf 'pw\\n'"), Secrets::new())];
        if let Ok(addr) = std::env::var("DBINE_TEST_VAULT") {
            let vault = SecretSource::Vault { address: addr, path: "secret/dbine/pg".into(), key: "password".into(), namespace: String::new(), kv_version: Default::default(), ca_file: String::new() };
            tries.push(("vault", vault, [(token_key("password"), "dbine-root".to_string())].into()));
        }
        for (what, src, tokens) in tries {
            let mut cfg = base.clone();
            let sources: SecretSources = [("password".to_string(), src)].into();
            fill(&store, &mut cfg, &sources, &tokens, Origin::Form { id: None, name: "" }).await.unwrap_or_else(|e| panic!("{what}: {e}"));
            let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap_or_else(|e| panic!("{what}: connect: {e}"));
            let v = s.server_version().await.unwrap();
            eprintln!("{what}: connected, {v}");
        }
    }
}

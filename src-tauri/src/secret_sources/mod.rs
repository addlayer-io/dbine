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
mod keepass;
mod onepassword;
mod vault;
mod zoho;

use crate::error::{CommandError, CommandResult};
use crate::state::{driver_info, field_applies, AppState};
use dbine_core::secret_sources::{self as defs, MasterSource, SecretSource, SecretSources, TOKEN_PREFIX};
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
    /// The `username` of the same secret (AWS, KeePass, Zoho Vault, when
    /// asked to fill it).
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

/// A master password the user is asked for (KeePass, Zoho Vault with
/// "ask"): the UI asks and answers with `unlock_secret_source`, then
/// retries. `wrong`: the one typed before didn't open it.
#[derive(Debug, Clone, Serialize)]
pub struct Unlock {
    pub connection_id: Option<String>,
    pub connection_name: String,
    pub field: String,
    pub field_label: String,
    /// `keepass` or `zoho`.
    pub kind: &'static str,
    pub wrong: bool,
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
        SecretSource::KeePass { .. } => "KeePass",
        SecretSource::Zoho { .. } => "Zoho Vault",
    }
}

/// `~/` is the home folder.
pub(crate) fn expand_home(p: &str) -> std::path::PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from);
    match (p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => std::path::PathBuf::from(p),
    }
}

/// Fetch couldn't go on without the master password it asks for: `fill`
/// turns this into an [`Unlock`].
pub const NEEDS_MASTER: &str = "se necesita la contraseña maestra";

/// A field's tokens by slot (`""`, `client_secret`…) out of a connection's.
pub fn field_tokens(tokens: &Secrets, field: &str) -> Secrets {
    tokens.iter().filter_map(|(k, v)| defs::token_parts(k).filter(|(f, _)| *f == field).map(|(_, slot)| (slot.to_string(), v.clone()))).collect()
}

/// The master password of a KeePass or Zoho Vault definition: kept, asked
/// (typed this run, else [`NEEDS_MASTER`]), a command's output, or none.
async fn master_password(src: &SecretSource, slots: &Secrets) -> Result<Option<String>, String> {
    let (master, command) = match src {
        SecretSource::KeePass { master, master_command, .. } | SecretSource::Zoho { master, master_command, .. } => (*master, master_command),
        _ => return Ok(None),
    };
    let typed = slots.get("").filter(|t| !t.is_empty()).cloned();
    match master {
        MasterSource::None => Ok(None),
        MasterSource::Keychain => typed.map(Some).ok_or_else(|| "falta la contraseña maestra: escribila en el formulario".to_string()),
        MasterSource::Ask => typed.map(Some).ok_or_else(|| NEEDS_MASTER.to_string()),
        MasterSource::Command => exec::run(exec::shell_command(command), exec::COMMAND_LIMIT)
            .await
            .map(exec::trim_newline)
            .map(Some)
            .map_err(|f| format!("la contraseña maestra: {}", command_failure(f))),
    }
}

/// The failure means "ask for the master password (again)".
fn asks_again(src: &SecretSource, message: &str) -> Option<bool> {
    if src.master() != Some(MasterSource::Ask) {
        return None;
    }
    match message {
        NEEDS_MASTER => Some(false),
        keepass::WRONG_KEY | zoho::WRONG_PASSPHRASE => Some(true),
        _ => None,
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

/// Ask the provider. `slots`: the field's tokens (the Vault token, the
/// master password, Zoho's client secret and refresh token), by slot.
pub async fn fetch(src: &SecretSource, slots: &Secrets) -> Result<Resolved, String> {
    let token = slots.get("").map(String::as_str);
    if let SecretSource::KeePass { file, entry, attribute, key_file, fill_username, .. } = src {
        let mut master = master_password(src, slots).await?;
        let req = keepass::Request {
            file: expand_home(file.trim()),
            entry: entry.clone(),
            attribute: attribute.clone(),
            key_file: Some(key_file.trim()).filter(|k| !k.is_empty()).map(expand_home),
            username: *fill_username,
        };
        let r = keepass::read(req, master.take()).await;
        return finish(src, r);
    }
    if let SecretSource::Zoho { data_center, secret_name, attribute, client_id, fill_username, .. } = src {
        let passphrase = master_password(src, slots).await?.unwrap_or_default();
        let creds = zoho::Creds {
            client_id: client_id.trim().to_string(),
            client_secret: slots.get("client_secret").cloned().unwrap_or_default(),
            refresh_token: slots.get("refresh_token").cloned().unwrap_or_default(),
            passphrase,
        };
        let req = zoho::Request { data_center, secret: secret_name, attribute, username: *fill_username };
        let r = zoho::read(&req, &creds).await;
        return finish(src, r);
    }
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
        SecretSource::KeePass { .. } | SecretSource::Zoho { .. } => unreachable!("resolved above"),
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

/// A provider that gives the value and the username itself.
fn finish(_src: &SecretSource, r: Result<(String, Option<String>), String>) -> Result<Resolved, String> {
    let (value, username) = r?;
    let resolved = Resolved { value, username };
    if resolved.value.is_empty() {
        return Err("el valor está vacío".into());
    }
    Ok(resolved)
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
#[cfg(test)]
pub fn token_key(field: &str) -> String {
    defs::token_key(field, "")
}

fn unlock_error(origin: Origin<'_>, info: Option<&DriverInfo>, field: &str, src: &SecretSource, wrong: bool) -> CommandError {
    CommandError::SecretSourceUnlock(Box::new(Unlock {
        connection_id: Some(origin.id().to_string()).filter(|i| !i.is_empty()),
        connection_name: origin.name().to_string(),
        field: field.to_string(),
        field_label: field_label(info, field),
        kind: src.kind(),
        wrong,
    }))
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
                let fetched = fetch(src, &field_tokens(tokens, field)).await.map_err(|m| match asks_again(src, &m) {
                    Some(wrong) => unlock_error(origin, info, field, src, wrong),
                    None => CommandError::SecretSource(format!("no se pudo obtener «{label}» de {}: {m}", provider(src))),
                })?;
                let r = Arc::new(fetched);
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
    /// Other tokens typed in the form, by slot (`""`: the master password;
    /// `client_secret`, `refresh_token`); the kept ones fill the rest.
    #[serde(default)]
    pub tokens: Option<Secrets>,
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
    let origin = Origin::Form { id, name: &name };
    check_trust(&state.store, origin, info, &args.field, &args.source)?;
    // Typed now, else kept (the keychain; a master password asked this run).
    let mut slots: Secrets = args.tokens.unwrap_or_default().into_iter().filter(|(_, v)| !v.is_empty()).collect();
    if let Some(t) = args.token.filter(|t| !t.is_empty()) {
        slots.insert(String::new(), t);
    }
    let wanted = args.source.token_slots();
    if let Some(i) = id {
        let mut known = Secrets::new();
        if wanted.iter().any(|s| !slots.contains_key(*s)) {
            known = secrets::get(i).unwrap_or_default();
        }
        if let Some(t) = state.typed_secrets.get(i) {
            known.extend(t.clone());
        }
        for (slot, v) in field_tokens(&known, &args.field) {
            if wanted.contains(&slot.as_str()) || (slot.is_empty() && args.source.master() == Some(MasterSource::Ask)) {
                slots.entry(slot).or_insert(v);
            }
        }
        for v in known.values_mut() {
            defs::wipe(v);
        }
    }
    let r = fetch(&args.source, &slots).await;
    for v in slots.values_mut() {
        defs::wipe(v);
    }
    let r = r.map_err(|m| match asks_again(&args.source, &m) {
        Some(wrong) => unlock_error(origin, info, &args.field, &args.source, wrong),
        None => CommandError::SecretSource(format!("no se pudo obtener «{}» de {}: {m}", field_label(info, &args.field), provider(&args.source))),
    })?;
    Ok(TestSecretSourceResult { chars: r.value.chars().count(), username: r.username.is_some() })
}

#[derive(Deserialize)]
pub struct UnlockSecretSourceArgs {
    pub connection_id: String,
    pub field: String,
    /// The master password typed in the dialog.
    pub value: String,
}

/// The master password asked for a saved connection's field: kept in
/// memory (with what else was typed for it) until it fails, the connection
/// is edited or DBine closes; never in the keychain.
#[tauri::command(rename_all = "camelCase")]
pub async fn unlock_secret_source(state: State<'_, AppState>, args: UnlockSecretSourceArgs) -> CommandResult<()> {
    if args.value.is_empty() {
        return Err(CommandError::BadRequest("falta la contraseña maestra".into()));
    }
    state.typed_secrets.entry(args.connection_id.clone()).or_default().insert(defs::token_key(&args.field, ""), args.value);
    forget_cached(&args.connection_id);
    Ok(())
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
        let r = fetch(&SecretSource::Command { command: r#"printf '{"username":"app","password":"pw"}'"#.into(), json_key: "password".into() }, &Secrets::new()).await.unwrap();
        assert_eq!((r.value.as_str(), r.username.as_deref()), ("pw", None));
        let e = fetch(&SecretSource::Command { command: "printf plain".into(), json_key: "password".into() }, &Secrets::new()).await.unwrap_err();
        assert!(e.contains("no es un objeto JSON") && !e.contains("plain"), "{e}");
        let e = fetch(&cmd("true"), &Secrets::new()).await.unwrap_err();
        assert_eq!(e, "el comando no devolvió nada");
        let e = fetch(&cmd("echo leaked-stdout; echo 'denied' >&2; exit 2"), &Secrets::new()).await.unwrap_err();
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

    fn kdbx(dir: &std::path::Path) -> std::path::PathBuf {
        let file = dir.join("db.kdbx");
        let db = keepass::tests::sample_db(::keepass::config::OuterCipherConfig::AES256, false);
        keepass::tests::save(&db, &file, Some("dummy-master"), None);
        file
    }

    fn kp(file: &std::path::Path, master: MasterSource, command: &str) -> SecretSource {
        SecretSource::KeePass {
            file: file.display().to_string(),
            entry: "Databases/Prod/pg-main".into(),
            attribute: String::new(),
            key_file: String::new(),
            master,
            master_command: command.into(),
            fill_username: true,
        }
    }

    /// The master password: kept, asked (an `Unlock` the UI answers), or a
    /// command's output; a wrong asked one asks again.
    #[cfg(unix)]
    #[tokio::test]
    async fn keepass_master_passwords() {
        let dir = temp_dir("kp-master");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let file = kdbx(&dir);
        let form = Origin::Form { id: None, name: "" };
        let base = ConnectionConfig { driver: "postgres".into(), host: "h".into(), ..Default::default() };
        let run = |src: SecretSource, tokens: Secrets| {
            let store = &store;
            let mut cfg = base.clone();
            async move {
                let sources: SecretSources = [("password".to_string(), src)].into();
                fill(store, &mut cfg, &sources, &tokens, form).await.map(|_| (cfg.password.unwrap_or_default(), cfg.username.unwrap_or_default()))
            }
        };
        let kept: Secrets = [(defs::token_key("password", ""), "dummy-master".to_string())].into();
        assert_eq!(run(kp(&file, MasterSource::Keychain, ""), kept.clone()).await.unwrap(), ("pg-secret-1".into(), "app_user".into()));
        let e = run(kp(&file, MasterSource::Keychain, ""), Secrets::new()).await.unwrap_err();
        assert!(e.to_string().contains("escribila en el formulario"), "{e}");
        // Asked: no password yet → Unlock; typed → opens; wrong → Unlock again.
        let e = run(kp(&file, MasterSource::Ask, ""), Secrets::new()).await.unwrap_err();
        let CommandError::SecretSourceUnlock(u) = &e else { panic!("{e:?}") };
        assert_eq!((u.kind, u.wrong, u.field.as_str()), ("keepass", false, "password"));
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["kind"], "secret_source_unlock");
        assert!(!json["message"].as_str().unwrap().contains(".kdbx"), "the message (what MCP sees) has no path");
        assert_eq!(run(kp(&file, MasterSource::Ask, ""), kept.clone()).await.unwrap().0, "pg-secret-1");
        let wrong: Secrets = [(defs::token_key("password", ""), "nope".to_string())].into();
        let e = run(kp(&file, MasterSource::Ask, ""), wrong.clone()).await.unwrap_err();
        assert!(matches!(&e, CommandError::SecretSourceUnlock(u) if u.wrong), "{e:?}");
        // Kept but wrong: an error, not a prompt.
        let e = run(kp(&file, MasterSource::Keychain, ""), wrong).await.unwrap_err();
        assert!(matches!(&e, CommandError::SecretSource(m) if m.contains(keepass::WRONG_KEY)), "{e:?}");
        // From a command.
        assert_eq!(run(kp(&file, MasterSource::Command, "printf 'dummy-master\\n'"), Secrets::new()).await.unwrap().0, "pg-secret-1");
        let e = run(kp(&file, MasterSource::Command, "echo denied >&2; exit 3"), Secrets::new()).await.unwrap_err();
        assert!(e.to_string().contains("la contraseña maestra: el comando terminó con el código 3: denied"), "{e}");
    }

    /// An imported KeePass definition points at a local file: it's not
    /// read until confirmed here, like the others.
    #[tokio::test]
    async fn imported_keepass_definitions_are_confirmed_first() {
        let dir = temp_dir("kp-import");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let file = kdbx(&dir);
        let src = kp(&file, MasterSource::Keychain, "");
        let mut cfg = ConnectionConfig { driver: "postgres".into(), host: "h".into(), ..Default::default() };
        let sources: SecretSources = [("password".to_string(), src.clone())].into();
        defs::write(&mut cfg, &sources);
        let id = format!("imported-kp-{}", uuid::Uuid::new_v4());
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
        defs::take(&mut cfg).unwrap();
        let kept: Secrets = [(defs::token_key("password", ""), "dummy-master".to_string())].into();
        for origin in [Origin::Saved { id: &id, name: "Imported" }, Origin::Form { id: None, name: "" }] {
            let e = fill(&store, &mut cfg, &sources, &kept, origin).await.unwrap_err();
            assert!(matches!(e, CommandError::SecretSourceApproval(_)), "{e:?}");
            assert_eq!(cfg.password, None);
        }
        defs::approve(&defs::digest(&id, "password", &src)).unwrap();
        fill(&store, &mut cfg, &sources, &kept, Origin::Saved { id: &id, name: "Imported" }).await.unwrap();
        assert_eq!(cfg.password.as_deref(), Some("pg-secret-1"));
        defs::forget(&[defs::digest(&id, "password", &src)]).unwrap();
        forget_cached(&id);
    }

    #[test]
    fn tokens_by_field_and_slot() {
        let all: Secrets = [
            (defs::token_key("password", ""), "m".to_string()),
            (defs::token_key("password", "refresh_token"), "r".to_string()),
            (defs::token_key("ssh.password", ""), "s".to_string()),
        ]
        .into();
        let p = field_tokens(&all, "password");
        assert_eq!(p.get("").map(String::as_str), Some("m"));
        assert_eq!(p.get("refresh_token").map(String::as_str), Some("r"));
        assert_eq!(p.len(), 2);
        assert_eq!(field_tokens(&all, "ssh.password").len(), 1);
    }

    /// End to end against `dbine-test-postgres` (port 25010, password `pw`):
    /// the password from a command, from a KeePass file written here (KDBX 4,
    /// Argon2, a key file; the user from the entry too) and from Vault
    /// (`dbine-test-vault`, `secret/dbine/pg` → `password`), then a real session.
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
        // KeePass: the entry has the user and the password.
        let mut db = keepass::tests::sample_db(::keepass::config::OuterCipherConfig::ChaCha20, false);
        db.root_mut().add_entry().edit(|e: &mut ::keepass::db::EntryMut<'_>| {
            e.set_unprotected(::keepass::db::fields::TITLE, "dbine-test-postgres");
            e.set_unprotected(::keepass::db::fields::USERNAME, "postgres");
            e.set_protected(::keepass::db::fields::PASSWORD, "pw");
        });
        let key_file = dir.join("db.keyx");
        std::fs::write(&key_file, b"dbine-live-test-key-file-0123456789").unwrap();
        let file = dir.join("live.kdbx");
        keepass::tests::save(&db, &file, Some("dummy-master"), Some(&key_file));
        let keepass = SecretSource::KeePass {
            file: file.display().to_string(),
            entry: "dbine-test-postgres".into(),
            attribute: String::new(),
            key_file: key_file.display().to_string(),
            master: MasterSource::Keychain,
            master_command: String::new(),
            fill_username: true,
        };
        tries.push(("keepass", keepass, [(token_key("password"), "dummy-master".to_string())].into()));
        if let Ok(addr) = std::env::var("DBINE_TEST_VAULT") {
            let vault = SecretSource::Vault { address: addr, path: "secret/dbine/pg".into(), key: "password".into(), namespace: String::new(), kv_version: Default::default(), ca_file: String::new() };
            tries.push(("vault", vault, [(token_key("password"), "dbine-root".to_string())].into()));
        }
        for (what, src, tokens) in tries {
            let mut cfg = base.clone();
            if what == "keepass" {
                cfg.username = Some("someone-else".into());
            }
            let sources: SecretSources = [("password".to_string(), src)].into();
            fill(&store, &mut cfg, &sources, &tokens, Origin::Form { id: None, name: "" }).await.unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(cfg.username.as_deref(), Some("postgres"), "{what}");
            let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap_or_else(|e| panic!("{what}: connect: {e}"));
            let v = s.server_version().await.unwrap();
            eprintln!("{what}: connected, {v}");
        }
    }
}

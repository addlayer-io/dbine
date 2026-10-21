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
mod cyberark;
mod delinea;
mod https;
mod vault_db;
mod bitwarden;
mod rsa_lc;
mod ssm;
pub mod browse;
pub mod accounts;

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
    /// The `username` of the same secret (AWS, KeePass, Zoho Vault,
    /// Bitwarden, Parameter Store, when asked to fill it).
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
    /// confirmed, a definition that came from outside doesn't. `duplicate_of`:
    /// the connection a new form was copied from ("Duplicar conexión"), whose
    /// unconfirmed definitions the copy can't be told apart from.
    Form { id: Option<&'a str>, duplicate_of: Option<&'a str>, name: &'a str },
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
    /// `keepass`, `zoho` or `bitwarden`.
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

/// Drop every resolved value (an account changed or was removed).
pub fn forget_all_cached() {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).clear();
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
        SecretSource::VaultDb { .. } => "HashiCorp Vault",
        SecretSource::Conjur { .. } => "CyberArk Conjur",
        SecretSource::Ccp { .. } => "CyberArk",
        SecretSource::Delinea { .. } => "Delinea Secret Server",
        SecretSource::Bitwarden { .. } => "Bitwarden",
        SecretSource::BitwardenSm { .. } => "Bitwarden Secrets Manager",
        SecretSource::Ssm { .. } => "AWS Systems Manager",
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
        SecretSource::KeePass { master, master_command, .. }
        | SecretSource::Zoho { master, master_command, .. }
        | SecretSource::Delinea { master, master_command, .. }
        | SecretSource::Bitwarden { master, master_command, .. } => (*master, master_command),
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
        keepass::WRONG_KEY | zoho::WRONG_PASSPHRASE | delinea::WRONG_PASSWORD | bitwarden::WRONG_PASSWORD => Some(true),
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
    if let SecretSource::VaultDb { .. } = src {
        // A one-off (Test): the credentials are revoked right away.
        let r = vault_db::test(&vault_db_source(src, slots)).await;
        return finish(src, r);
    }
    if let SecretSource::Conjur { url, account, login, variable, username_variable, ca_file, .. } = src {
        let req = cyberark::Conjur { url, account, login, variable, username_variable, ca_file };
        let (mut raw, username) = cyberark::conjur_read(&req, token).await?;
        let picked = defs::pick_json(&raw, src.json_key()).map_err(|e| e.to_string());
        defs::wipe(&mut raw);
        return finish(src, picked.map(|v| (v, username)));
    }
    if let SecretSource::Ccp { url, app_id, safe, folder, object, account_user, account_address, client_cert, client_key, ca_file, fill_username, .. } = src {
        let req = cyberark::Ccp { url, app_id, safe, folder, object, account_user, account_address, client_cert, client_key, ca_file, username: *fill_username };
        return finish(src, cyberark::ccp_read(&req, token).await);
    }
    if let SecretSource::Delinea { url, platform_url, username, domain, secret_name, attribute, fill_username, user_field, .. } = src {
        let mut password = master_password(src, slots).await?.unwrap_or_default();
        let req = delinea::Request { url, platform_url, username, domain, secret: secret_name, field: attribute, fill_username: *fill_username, user_field };
        let r = delinea::read(&req, &password).await;
        defs::wipe(&mut password);
        return finish(src, r);
    }
    if let SecretSource::KeePass { file, entry, attribute, key_file, fill_username, user_field, .. } = src {
        let mut master = master_password(src, slots).await?;
        let req = keepass::Request {
            file: expand_home(file.trim()),
            entry: entry.clone(),
            attribute: attribute.clone(),
            key_file: Some(key_file.trim()).filter(|k| !k.is_empty()).map(expand_home),
            username: *fill_username,
            user_field: user_field.clone(),
        };
        let r = keepass::read(req, master.take()).await;
        return finish(src, r);
    }
    if let SecretSource::Zoho { data_center, secret_name, attribute, client_id, fill_username, user_field, .. } = src {
        let passphrase = master_password(src, slots).await?.unwrap_or_default();
        let creds = zoho::Creds {
            client_id: client_id.trim().to_string(),
            client_secret: slots.get("client_secret").cloned().unwrap_or_default(),
            refresh_token: slots.get("refresh_token").cloned().unwrap_or_default(),
            passphrase,
        };
        let req = zoho::Request { data_center, secret: secret_name, attribute, username: *fill_username, user_field };
        let r = zoho::read(&req, &creds).await;
        return finish(src, r);
    }
    if let SecretSource::Bitwarden { server, item, attribute, client_id, fill_username, user_field, .. } = src {
        let password = master_password(src, slots).await?.unwrap_or_default();
        let creds = bitwarden::Creds { client_id: client_id.trim().to_string(), client_secret: slots.get("client_secret").cloned().unwrap_or_default(), password };
        let req = bitwarden::Request { server, item, attribute, username: *fill_username, user_field };
        // The notes (or any field) may be JSON: its key.
        let r = bitwarden::read(&req, &creds).await.and_then(|(mut raw, mut username)| {
            let picked = defs::pick_json(&raw, src.json_key()).map_err(|e| e.to_string());
            defs::wipe(&mut raw);
            if picked.is_err() {
                if let Some(u) = &mut username {
                    defs::wipe(u);
                }
            }
            picked.map(|v| (v, username))
        });
        return finish(src, r);
    }
    let mut raw = match src {
        SecretSource::Command { command, .. } => exec::run(exec::shell_command(command), exec::COMMAND_LIMIT).await.map(exec::trim_newline).map_err(command_failure)?,
        SecretSource::OnePassword { reference, account, fill_username, user_field, .. } => {
            if *fill_username {
                // One `op` call for both: one unlock prompt.
                let (mut value, mut user) = onepassword::read_with_user(reference, user_field, account).await?;
                let picked = defs::pick_json(&value, src.json_key()).map_err(|e| e.to_string());
                defs::wipe(&mut value);
                if picked.is_err() {
                    defs::wipe(&mut user);
                }
                return finish(src, picked.map(|v| (v, Some(user))));
            }
            onepassword::read(reference, account).await?
        }
        SecretSource::Vault { address, path, key, namespace, kv_version, ca_file, auth, approle_mount, role_id, .. } => {
            let req = vault::Request { address, path, key, namespace, kv_version: *kv_version, ca_file };
            vault::read(&req, &vault::Auth::of(*auth, approle_mount, role_id, slots)).await?
        }
        SecretSource::Aws { secret_id, region, profile, .. } => aws::get_secret(secret_id, region, profile).await?,
        SecretSource::Azure { vault_url, name, version, tenant, .. } => azure::get_secret(vault_url, name, version, tenant).await?,
        SecretSource::Gcp { project, secret, version, account, .. } => gcp::get_secret(project, secret, version, account).await?,
        SecretSource::BitwardenSm { server, secret, .. } => bitwarden::read_secret(server, secret, token).await?,
        SecretSource::Ssm { parameter, version, region, profile, list_index, .. } => ssm::get_parameter(&ssm::Request { parameter, version, region, profile, list_index }).await?,
        SecretSource::KeePass { .. }
        | SecretSource::Zoho { .. }
        | SecretSource::VaultDb { .. }
        | SecretSource::Conjur { .. }
        | SecretSource::Ccp { .. }
        | SecretSource::Delinea { .. }
        | SecretSource::Bitwarden { .. } => unreachable!("resolved above"),
    };
    let picked = defs::pick_json(&raw, src.json_key()).map_err(|e| e.to_string());
    let username = match src {
        SecretSource::Aws { fill_username: true, .. } | SecretSource::Ssm { fill_username: true, .. } => Some(defs::pick_json(&raw, "username").map_err(|e| e.to_string())),
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

/// A Vault database role's definition, with the field's kept slots.
fn vault_db_source<'a>(src: &'a SecretSource, slots: &'a Secrets) -> vault_db::Source<'a> {
    let SecretSource::VaultDb { address, mount, role, static_role, namespace, ca_file, auth, approle_mount, role_id, .. } = src else {
        unreachable!("a vault_db definition")
    };
    vault_db::Source {
        req: vault_db::Request { address, namespace, ca_file, mount, role, static_role: *static_role },
        auth: vault::Auth::of(*auth, approle_mount, role_id, slots),
    }
}

/// Revoke the Vault leases a connection holds (disconnect, edit, delete):
/// in the background, best effort (crate::state::close_connection_sessions).
pub fn release_leases(connection_id: &str) {
    let id = connection_id.to_string();
    tauri::async_runtime::spawn(async move { vault_db::release(&id).await });
}

/// Run a form's test connection (resolve, connect, close the session) and
/// revoke the Vault database leases it got as soon as it ends.
pub async fn with_form_leases<F: std::future::Future>(f: F) -> F::Output {
    vault_db::form_scope(f).await
}

/// Revoke every Vault lease held (DBine is closing).
pub async fn release_all_leases() {
    vault_db::release_all().await;
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
        // The edited connection's own unconfirmed definition, or one that
        // came from outside. Saved connections that can't be listed: nothing
        // can be told, so it asks.
        Origin::Form { id, duplicate_of, .. } => match store.list_connections() {
            Ok(saved) => !defs::needs_confirmation(&saved, id, duplicate_of, field, src),
            Err(_) => false,
        },
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
            // Vault's database credentials: held with their lease while the
            // connection is open (vault_db.rs), not in the minute's cache.
            None if matches!(src, SecretSource::VaultDb { .. }) => {
                // Through an account: the account's Vault and sign-in.
                let x = accounts::expand(store, src, field_tokens(tokens, field))
                    .map_err(|m| CommandError::SecretSource(format!("no se pudo obtener «{label}» de {}: {m}", provider(src))))?;
                let key = if saved { key } else { format!("form:{}\u{0}{digest}", uuid::Uuid::new_v4()) };
                vault_db::acquire(&key, saved, &vault_db_source(&x.src, &x.slots))
                    .await
                    .map_err(|m| CommandError::SecretSource(format!("no se pudo obtener «{label}» de {}: {m}", provider(src))))?
            }
            Some(r) => r,
            None => {
                // A definition naming an account runs with the account's sign-in.
                let x = accounts::expand(store, src, field_tokens(tokens, field))
                    .map_err(|m| CommandError::SecretSource(format!("no se pudo obtener «{label}» de {}: {m}", provider(src))))?;
                let r = fetch(&x.src, &x.slots).await;
                accounts::after_run(&x, r.as_ref().map(|_| ()).map_err(String::as_str));
                let fetched = r.map_err(|m| match asks_again(&x.src, &m) {
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

/// A form's tokens for a field: typed now (`typed`, `token` the main one),
/// else kept for the saved connection (the keychain; a master password
/// asked this run).
fn form_slots(state: &AppState, id: Option<&str>, field: &str, src: &SecretSource, typed: Secrets, token: Option<String>) -> Secrets {
    let mut slots: Secrets = typed.into_iter().filter(|(_, v)| !v.is_empty()).collect();
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        slots.insert(String::new(), t);
    }
    let wanted = src.token_slots();
    if let Some(i) = id {
        let mut known = Secrets::new();
        if wanted.iter().any(|s| !slots.contains_key(*s)) {
            known = secrets::get(i).unwrap_or_default();
        }
        if let Some(t) = state.typed_secrets.get(i) {
            known.extend(t.clone());
        }
        for (slot, v) in field_tokens(&known, field) {
            if wanted.contains(&slot.as_str()) || (slot.is_empty() && src.master() == Some(MasterSource::Ask)) {
                slots.entry(slot).or_insert(v);
            }
        }
        for v in known.values_mut() {
            defs::wipe(v);
        }
    }
    slots
}

#[derive(Deserialize)]
pub struct TestSecretSourceArgs {
    /// The connection being edited, if it's saved.
    pub connection_id: Option<String>,
    /// The connection a new form was copied from ("Duplicar conexión").
    #[serde(default)]
    pub duplicate_of: Option<String>,
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
    let origin = Origin::Form { id, duplicate_of: args.duplicate_of.as_deref().filter(|i| !i.is_empty()), name: &name };
    check_trust(&state.store, origin, info, &args.field, &args.source)?;
    let slots = form_slots(&state, id, &args.field, &args.source, args.tokens.unwrap_or_default(), args.token);
    let label = field_label(info, &args.field);
    let x = accounts::expand(&state.store, &args.source, slots).map_err(|m| CommandError::SecretSource(format!("no se pudo obtener «{label}» de {}: {m}", provider(&args.source))))?;
    let r = fetch(&x.src, &x.slots).await;
    accounts::after_run(&x, r.as_ref().map(|_| ()).map_err(String::as_str));
    let r = r.map_err(|m| match asks_again(&x.src, &m) {
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
        assert!(fill(&store, &mut run_cfg, &sources, &Secrets::new(), Origin::Form { id: Some(&id), duplicate_of: None, name: "Imported" }).await.is_err());
        // A copy in a new form: refused (same text as the unconfirmed one).
        assert!(fill(&store, &mut run_cfg, &sources, &Secrets::new(), Origin::Form { id: None, duplicate_of: None, name: "" }).await.is_err());
        assert!(!marker.exists());
        // Padded (the form trims it): still the same definition, refused.
        let padded: SecretSources = [("password".to_string(), cmd(&format!(" touch '{}'; printf pw ", marker.display())))].into();
        for origin in [Origin::Form { id: Some(&id), duplicate_of: None, name: "Imported" }, Origin::Form { id: None, duplicate_of: None, name: "" }] {
            assert!(matches!(fill(&store, &mut run_cfg, &padded, &Secrets::new(), origin).await, Err(CommandError::SecretSourceApproval(_))));
        }
        assert!(!marker.exists());
        // Anything else typed over the unconfirmed field of the connection
        // it came with: asks too (the form rewrites what it loads).
        let typed: SecretSources = [("password".to_string(), cmd(&format!("touch '{}'; printf typed", marker.display())))].into();
        let e = fill(&store, &mut run_cfg, &typed, &Secrets::new(), Origin::Form { id: Some(&id), duplicate_of: None, name: "Imported" }).await.unwrap_err();
        assert!(matches!(e, CommandError::SecretSourceApproval(_)), "{e:?}");
        assert!(!marker.exists());
        // Typed in a new form: runs.
        fill(&store, &mut run_cfg, &typed, &Secrets::new(), Origin::Form { id: None, duplicate_of: None, name: "" }).await.unwrap();
        assert_eq!(run_cfg.password.as_deref(), Some("typed"));
        std::fs::remove_file(&marker).unwrap();
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

    /// An imported command padded with a BOM (which the form's JavaScript
    /// trims and Rust's `trim` didn't), opened with "Duplicar conexión":
    /// the copy's Test, field Test and Examinar ask, and nothing runs.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_duplicated_bom_padded_import_asks_and_never_runs() {
        let dir = temp_dir("bom-dup");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let marker = dir.join("ran");
        let body = format!("touch '{}'; printf pw", marker.display());
        // As stored by the import: the BOM is in the state, not validated.
        let mut cfg = ConnectionConfig { driver: "postgres".into(), host: "h".into(), ..Default::default() };
        let json = serde_json::json!({ "password": { "kind": "command", "command": format!("\u{FEFF}{body}") } }).to_string();
        cfg.options.insert(defs::OPTION.into(), json);
        let id = format!("imported-bom-{}", uuid::Uuid::new_v4());
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
        // What the copy's form sends: trimmed by JavaScript (no BOM), or edited.
        let mut run_cfg = ConnectionConfig { driver: "postgres".into(), host: "h".into(), ..Default::default() };
        let copy = Origin::Form { id: None, duplicate_of: Some(&id), name: "" };
        for text in [body.clone(), format!("{body}; printf more")] {
            let sources: SecretSources = [("password".to_string(), cmd(&text))].into();
            let e = fill(&store, &mut run_cfg, &sources, &Secrets::new(), copy).await.unwrap_err();
            assert!(matches!(e, CommandError::SecretSourceApproval(_)), "{e:?}");
            let e = check_trust(&store, copy, None, "password", &cmd(&text)).unwrap_err();
            assert!(matches!(e, CommandError::SecretSourceApproval(_)), "{e:?}");
            let e = browse::may_browse(&store, None, Some(&id), None, "password", &cmd(&text)).unwrap_err();
            assert!(matches!(e, CommandError::SecretSourceApproval(_)), "{e:?}");
        }
        // The trimmed text is the stored definition's canonical form: a new
        // form (not a copy) asks for it too.
        let sources: SecretSources = [("password".to_string(), cmd(&body))].into();
        let new_form = Origin::Form { id: None, duplicate_of: None, name: "" };
        assert!(matches!(fill(&store, &mut run_cfg, &sources, &Secrets::new(), new_form).await, Err(CommandError::SecretSourceApproval(_))));
        // The stored one with its BOM doesn't run as a saved connection: an
        // invisible character isn't a valid definition.
        let stored: SecretSources = defs::read(&cfg).unwrap();
        let e = fill(&store, &mut run_cfg, &stored, &Secrets::new(), Origin::Saved { id: &id, name: "Imported" }).await.unwrap_err();
        assert!(e.to_string().contains("caracteres invisibles"), "{e}");
        assert!(!marker.exists(), "nothing ran");
        assert_eq!(run_cfg.password, None);
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
            user_field: String::new(),
            secret_account: String::new(),
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
        let form = Origin::Form { id: None, duplicate_of: None, name: "" };
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
        for origin in [Origin::Saved { id: &id, name: "Imported" }, Origin::Form { id: None, duplicate_of: None, name: "" }] {
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

    // ---- Vault's database credentials against a mock -----------------------------

    #[derive(Default)]
    struct VaultMock {
        issued: Mutex<u32>,
        /// The next leases' duration and whether they can be renewed.
        ttl: Mutex<(u64, bool)>,
        renewed: Mutex<Vec<String>>,
        revoked: Mutex<Vec<String>>,
        tokens_revoked: Mutex<u32>,
        /// The tokens each call came with.
        tokens: Mutex<Vec<String>>,
    }

    async fn vault_mock() -> (String, Arc<VaultMock>) {
        use axum::extract::{Path as AxPath, State as AxState};
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::{get, post, put};
        use axum::{Json, Router};
        use serde_json::{json, Value};
        let m = Arc::new(VaultMock { ttl: Mutex::new((3600, true)), ..Default::default() });
        let tok = |h: &HeaderMap| h.get("x-vault-token").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let app = Router::new()
            .route(
                "/v1/database/creds/{role}",
                get(move |AxState(m): AxState<Arc<VaultMock>>, AxPath(role): AxPath<String>, h: HeaderMap| async move {
                    let t = tok(&h);
                    m.tokens.lock().unwrap().push(t.clone());
                    if t != "root" && t != "at-approle" {
                        return (StatusCode::FORBIDDEN, Json(json!({ "errors": ["permission denied"] })));
                    }
                    if role != "ro" {
                        return (StatusCode::BAD_REQUEST, Json(json!({ "errors": [format!("unknown role: {role}")] })));
                    }
                    let n = {
                        let mut i = m.issued.lock().unwrap();
                        *i += 1;
                        *i
                    };
                    let (ttl, renewable) = *m.ttl.lock().unwrap();
                    (StatusCode::OK, Json(json!({ "lease_id": format!("database/creds/ro/L{n}"), "lease_duration": ttl, "renewable": renewable,
                        "data": { "username": format!("v-ro-{n}"), "password": format!("pw-{n}") } })))
                }),
            )
            .route(
                "/v1/database/roles",
                get(move |AxState(_m): AxState<Arc<VaultMock>>, h: HeaderMap| async move {
                    if tok(&h) != "root" {
                        return (StatusCode::FORBIDDEN, Json(json!({ "errors": ["permission denied"] })));
                    }
                    (StatusCode::OK, Json(json!({ "data": { "keys": ["ro", "admin"] } })))
                }),
            )
            .route(
                "/v1/sys/leases/renew",
                put(|AxState(m): AxState<Arc<VaultMock>>, Json(b): Json<Value>| async move {
                    m.renewed.lock().unwrap().push(b["lease_id"].as_str().unwrap_or("").to_string());
                    (StatusCode::OK, Json(json!({ "lease_id": b["lease_id"], "lease_duration": 3600, "renewable": true })))
                }),
            )
            .route(
                "/v1/sys/leases/revoke",
                put(|AxState(m): AxState<Arc<VaultMock>>, Json(b): Json<Value>| async move {
                    m.revoked.lock().unwrap().push(b["lease_id"].as_str().unwrap_or("").to_string());
                    StatusCode::NO_CONTENT
                }),
            )
            .route(
                "/v1/auth/approle/login",
                post(|Json(b): Json<Value>| async move {
                    if b["role_id"] == "role-1" && b["secret_id"] == "dummy-secret-id" {
                        (StatusCode::OK, Json(json!({ "auth": { "client_token": "at-approle", "lease_duration": 1200, "renewable": true } })))
                    } else {
                        (StatusCode::BAD_REQUEST, Json(json!({ "errors": ["invalid role or secret ID"] })))
                    }
                }),
            )
            .route(
                "/v1/auth/token/revoke-self",
                post(|AxState(m): AxState<Arc<VaultMock>>| async move {
                    *m.tokens_revoked.lock().unwrap() += 1;
                    StatusCode::NO_CONTENT
                }),
            )
            .with_state(m.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), m)
    }

    fn vault_db_src(address: &str, role: &str, auth: defs::VaultAuth) -> SecretSource {
        SecretSource::VaultDb {
            address: address.into(),
            mount: String::new(),
            role: role.into(),
            static_role: false,
            namespace: String::new(),
            ca_file: String::new(),
            auth,
            approle_mount: String::new(),
            role_id: if auth == defs::VaultAuth::Approle { "role-1".into() } else { String::new() },
            secret_account: String::new(),
        }
    }

    /// "Examinar": a Vault database engine's roles, through the same
    /// dispatch the command uses; nothing is issued.
    #[tokio::test]
    async fn browse_vault_database_roles() {
        let (addr, m) = vault_mock().await;
        let src = vault_db_src(&addr, "", defs::VaultAuth::Token);
        let slots: Secrets = [(String::new(), "root".to_string())].into();
        let l = browse::list(&src, &slots, Default::default()).await.unwrap();
        let shown: Vec<(&str, &str)> = l.entries.iter().map(|e| (e.path.as_str(), e.id.as_str())).collect();
        assert_eq!(shown, [("roles", "admin"), ("roles", "ro")], "no static roles here (404)");
        assert_eq!(*m.issued.lock().unwrap(), 0, "browsing issues no credentials");
        let none: Secrets = [(String::new(), "nope".to_string())].into();
        assert!(browse::list(&src, &none, Default::default()).await.unwrap_err().contains("permiso denegado"));
    }

    /// Browsing what came from outside asks to confirm it first: the form's
    /// definition when it's the imported one, and anything while the edited
    /// connection's own definition isn't confirmed.
    #[test]
    fn browsing_imported_definitions_is_confirmed_first() {
        let dir = temp_dir("browse-trust");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let src = SecretSource::KeePass {
            file: format!("{}/imported.kdbx", dir.display()),
            entry: "Databases/pg".into(),
            attribute: String::new(),
            key_file: String::new(),
            master: MasterSource::Ask,
            master_command: String::new(),
            fill_username: false,
            user_field: String::new(),
            secret_account: String::new(),
        };
        let mut cfg = ConnectionConfig { driver: "postgres".into(), ..Default::default() };
        defs::write(&mut cfg, &[("password".to_string(), src.clone())].into());
        let saved = dbine_core::SavedConnection { id: format!("imported-browse-{}", uuid::Uuid::new_v4()), name: "imp".into(), color: None, config: cfg, save_password: true, folder_id: None, tags: vec![], mcp_level: None, updated_at: String::new() };
        store.save_connection(&saved).unwrap();
        // A new form, the imported definition typed as is: confirm it first.
        assert!(matches!(browse::may_browse(&store, None, None, None, "password", &src), Err(CommandError::SecretSourceApproval(_))));
        // Editing the imported connection, even with another definition.
        let mut other = src.clone();
        if let SecretSource::KeePass { file, .. } = &mut other {
            *file = "~/mine.kdbx".into();
        }
        let e = browse::may_browse(&store, Some(&saved), None, None, "password", &other).unwrap_err();
        assert!(matches!(&e, CommandError::SecretSourceApproval(a) if a.source == src), "the saved one is shown: {e:?}");
        // Confirmed: browsing goes on.
        defs::approve(&defs::digest(&saved.id, "password", &src)).unwrap();
        assert!(browse::may_browse(&store, Some(&saved), None, None, "password", &other).is_ok());
        assert!(browse::may_browse(&store, None, None, None, "password", &other).is_ok(), "typed in a new form");
        defs::forget(&[defs::digest(&saved.id, "password", &src)]).unwrap();
    }

    /// The lease's life against a mock of Vault: one lease for the user and
    /// the password, shared by the sessions of a connection; renewed near
    /// its end; replaced when it can't be; every one revoked when the
    /// connection closes; Test revokes at once; AppRole signs in and its
    /// token is revoked too.
    #[tokio::test]
    async fn vault_database_leases_follow_the_connection() {
        let (addr, m) = vault_mock().await;
        let dir = temp_dir("vault-db");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let id = format!("vdb-{}", uuid::Uuid::new_v4());
        let src = vault_db_src(&addr, "ro", defs::VaultAuth::Token);
        let sources: SecretSources = [("password".to_string(), src.clone())].into();
        defs::approve(&defs::digest(&id, "password", &src)).unwrap();
        let root: Secrets = [(defs::token_key("password", ""), "root".to_string())].into();
        let base = ConnectionConfig { driver: "postgres".into(), host: "h".into(), username: Some("typed-user".into()), ..Default::default() };
        let saved = Origin::Saved { id: &id, name: "Vault DB" };
        let connect = || {
            let (store, sources, root, base) = (&store, &sources, &root, base.clone());
            async move {
                let mut cfg = base;
                fill(store, &mut cfg, sources, root, saved).await.map(|_| (cfg.username.unwrap_or_default(), cfg.password.unwrap_or_default()))
            }
        };
        // Two sessions: one lease, its user and password together.
        assert_eq!(connect().await.unwrap(), ("v-ro-1".into(), "pw-1".into()));
        assert_eq!(connect().await.unwrap(), ("v-ro-1".into(), "pw-1".into()));
        assert_eq!(*m.issued.lock().unwrap(), 1);
        assert_eq!(vault_db::held_count(&id), (1, 0));
        // A lease about to end is renewed (and reused).
        vault_db::release(&id).await;
        *m.ttl.lock().unwrap() = (5, true);
        assert_eq!(connect().await.unwrap().1, "pw-2");
        assert_eq!(connect().await.unwrap().1, "pw-2", "renewed, not replaced");
        assert_eq!(m.renewed.lock().unwrap().as_slice(), ["database/creds/ro/L2"]);
        // One that can't be renewed is replaced; the old one waits for the
        // connection to close (a session may still use it).
        vault_db::release(&id).await;
        *m.ttl.lock().unwrap() = (5, false);
        assert_eq!(connect().await.unwrap().1, "pw-3");
        assert_eq!(connect().await.unwrap().1, "pw-4");
        assert_eq!(vault_db::held_count(&id), (1, 1));
        // Disconnect: everything revoked.
        vault_db::release(&id).await;
        assert_eq!(vault_db::held_count(&id), (0, 0));
        let mut revoked = m.revoked.lock().unwrap().clone();
        revoked.sort();
        assert_eq!(revoked, ["database/creds/ro/L1", "database/creds/ro/L2", "database/creds/ro/L3", "database/creds/ro/L4"]);
        // After a disconnect, new credentials.
        *m.ttl.lock().unwrap() = (3600, true);
        assert_eq!(connect().await.unwrap().1, "pw-5");
        vault_db::release(&id).await;
        // Test: issued and revoked at once, never held.
        let r = fetch(&src, &field_tokens(&root, "password")).await.unwrap();
        assert_eq!((r.value.as_str(), r.username.as_deref()), ("pw-6", Some("v-ro-6")));
        assert_eq!(m.revoked.lock().unwrap().last().map(String::as_str), Some("database/creds/ro/L6"));
        // Errors carry Vault's reason, never a value.
        let e = fetch(&vault_db_src(&addr, "nope", defs::VaultAuth::Token), &field_tokens(&root, "password")).await.unwrap_err();
        assert!(e.contains("no existe el rol «nope»"), "{e}");
        let bad: Secrets = [(String::new(), "wrong".to_string())].into();
        assert!(fetch(&src, &bad).await.unwrap_err().contains("permiso denegado"));
        // AppRole: signs in with the role ID and the kept secret ID; the
        // token goes with the lease and is revoked after it.
        let approle = vault_db_src(&addr, "ro", defs::VaultAuth::Approle);
        let ar_sources: SecretSources = [("password".to_string(), approle.clone())].into();
        defs::approve(&defs::digest(&id, "password", &approle)).unwrap();
        let sid: Secrets = [(defs::token_key("password", "secret_id"), "dummy-secret-id".to_string())].into();
        let mut cfg = base.clone();
        fill(&store, &mut cfg, &ar_sources, &sid, saved).await.unwrap();
        assert_eq!(cfg.username.as_deref(), Some("v-ro-7"));
        assert_eq!(m.tokens.lock().unwrap().last().map(String::as_str), Some("at-approle"));
        let before = *m.tokens_revoked.lock().unwrap();
        vault_db::release(&id).await;
        assert_eq!(*m.tokens_revoked.lock().unwrap(), before + 1, "the AppRole token too");
        let wrong: Secrets = [(defs::token_key("password", "secret_id"), "nope".to_string())].into();
        let e = fill(&store, &mut base.clone(), &ar_sources, &wrong, saved).await.unwrap_err().to_string();
        assert!(e.contains("role_id o el secret_id") && !e.contains("nope"), "{e}");
        defs::forget(&[defs::digest(&id, "password", &src), defs::digest(&id, "password", &approle)]).unwrap();
    }

    /// "Probar conexión": the credentials the form's test leased are revoked
    /// as soon as the test ends (its session closed), not two minutes later.
    #[tokio::test]
    async fn vault_database_test_connection_revokes_at_once() {
        let (addr, m) = vault_mock().await;
        let dir = temp_dir("vault-db-test-conn");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let src = vault_db_src(&addr, "ro", defs::VaultAuth::Token);
        let sources: SecretSources = [("password".to_string(), src)].into();
        let root: Secrets = [(defs::token_key("password", ""), "root".to_string())].into();
        let base = ConnectionConfig { driver: "postgres".into(), host: "h".into(), ..Default::default() };
        let form = Origin::Form { id: None, duplicate_of: None, name: "" };
        let m2 = m.clone();
        let user = with_form_leases(async {
            let mut cfg = base.clone();
            fill(&store, &mut cfg, &sources, &root, form).await.unwrap();
            // While the test's session is open, the lease is still there.
            assert!(m2.revoked.lock().unwrap().is_empty(), "not revoked while the test runs");
            cfg.username.unwrap_or_default()
        })
        .await;
        let lease = format!("database/creds/ro/L{}", user.trim_start_matches("v-ro-"));
        assert_eq!(m.revoked.lock().unwrap().as_slice(), [lease], "revoked right after the test");
        // Outside a test's scope (a form's other uses) nothing changes: the
        // two-minute fallback still applies, nothing is revoked yet.
        let mut cfg = base.clone();
        fill(&store, &mut cfg, &sources, &root, form).await.unwrap();
        assert_eq!(m.revoked.lock().unwrap().len(), 1);
    }

    /// AppRole for a KV read: signs in, reads, revokes its token.
    #[tokio::test]
    async fn approle_signs_in_for_kv_too() {
        let (addr, m) = vault_mock().await;
        let client = reqwest::Client::new();
        let conn = vault::Conn { address: &addr, namespace: "" };
        let slots: Secrets = [("secret_id".to_string(), "dummy-secret-id".to_string())].into();
        let login = vault::login(&client, &conn, &vault::Auth::of(defs::VaultAuth::Approle, "", "role-1", &slots)).await.unwrap();
        assert_eq!((login.token.as_str(), login.owned), ("at-approle", true));
        login.done(&client, &conn).await;
        assert_eq!(*m.tokens_revoked.lock().unwrap(), 1);
        let none = Secrets::new();
        let e = vault::login(&client, &conn, &vault::Auth::of(defs::VaultAuth::Approle, "", "role-1", &none)).await.err().unwrap();
        assert!(e.contains("AppRole"), "{e}");
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
            user_field: String::new(),
            secret_account: String::new(),
        };
        tries.push(("keepass", keepass, [(token_key("password"), "dummy-master".to_string())].into()));
        if let Ok(addr) = std::env::var("DBINE_TEST_VAULT") {
            let vault = SecretSource::Vault {
                address: addr,
                path: "secret/dbine/pg".into(),
                key: "password".into(),
                namespace: String::new(),
                kv_version: Default::default(),
                ca_file: String::new(),
                auth: Default::default(),
                approle_mount: String::new(),
                role_id: String::new(),
                secret_account: String::new(),
            };
            tries.push(("vault", vault, [(token_key("password"), "dbine-root".to_string())].into()));
        }
        for (what, src, tokens) in tries {
            let mut cfg = base.clone();
            if what == "keepass" {
                cfg.username = Some("someone-else".into());
            }
            let sources: SecretSources = [("password".to_string(), src)].into();
            fill(&store, &mut cfg, &sources, &tokens, Origin::Form { id: None, duplicate_of: None, name: "" }).await.unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(cfg.username.as_deref(), Some("postgres"), "{what}");
            let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap_or_else(|e| panic!("{what}: connect: {e}"));
            let v = s.server_version().await.unwrap();
            eprintln!("{what}: connected, {v}");
        }
    }

    /// Vault's database secrets engine against `dbine-test-postgres`: a
    /// real session opens with the credentials Vault made, the role exists
    /// while the connection is open and is gone after the disconnect (the
    /// lease is revoked); with a token and with AppRole; and a static role.
    /// Setup (`dbine-test-vault-db`, dev mode, the `database` engine with
    /// the roles `ro` and `static-app`, AppRole `dbine`): docs/secret-sources.md,
    /// "For developers". Then
    /// `DBINE_TEST_VAULT_DB=http://127.0.0.1:28201 DBINE_TEST_VAULT_APPROLE=<role_id>:<secret_id> cargo test -p dbine secret_sources::tests::live_vault_database -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_vault_database() {
        let Ok(addr) = std::env::var("DBINE_TEST_VAULT_DB") else { return };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = temp_dir("vault-db-live");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        // Asked to the server itself, as its superuser, in the container.
        let role_exists = async |name: &str| {
            let q = format!("SELECT count(*) FROM pg_roles WHERE rolname = '{}'", name.replace('\'', "''"));
            let out = tokio::process::Command::new("docker").args(["exec", "dbine-test-postgres", "psql", "-U", "postgres", "-tAc", &q]).output().await.unwrap();
            String::from_utf8_lossy(&out.stdout).trim() == "1"
        };
        let base = ConnectionConfig { driver: "postgres".into(), host: "127.0.0.1".into(), port: 25010, database: "postgres".into(), username: Some("someone-else".into()), ..Default::default() };
        let mut tries: Vec<(&str, SecretSource, Secrets)> = vec![("token", vault_db_src(&addr, "ro", defs::VaultAuth::Token), [(defs::token_key("password", ""), "dbine-root".to_string())].into())];
        if let Some((role_id, secret_id)) = std::env::var("DBINE_TEST_VAULT_APPROLE").ok().and_then(|v| v.split_once(':').map(|(a, b)| (a.to_string(), b.to_string()))) {
            let mut src = vault_db_src(&addr, "ro", defs::VaultAuth::Approle);
            if let SecretSource::VaultDb { role_id: r, approle_mount, .. } = &mut src {
                *r = role_id;
                *approle_mount = "approle".into();
            }
            tries.push(("approle", src, [(defs::token_key("password", "secret_id"), secret_id)].into()));
        }
        for (what, src, tokens) in tries {
            let id = format!("vdb-live-{what}-{}", uuid::Uuid::new_v4());
            let sources: SecretSources = [("password".to_string(), src.clone())].into();
            defs::approve(&defs::digest(&id, "password", &src)).unwrap();
            let origin = Origin::Saved { id: &id, name: "live" };
            let mut cfg = base.clone();
            fill(&store, &mut cfg, &sources, &tokens, origin).await.unwrap_or_else(|e| panic!("{what}: {e}"));
            let user = cfg.username.clone().unwrap();
            assert!(user.starts_with("v-"), "{what}: a user Vault made: {user}");
            let mut s1 = dbine_drivers::open_session(&cfg, None).await.unwrap_or_else(|e| panic!("{what}: connect: {e}"));
            eprintln!("{what}: connected as a Vault user, {}", s1.server_version().await.unwrap());
            // A second session of the same connection: the same user.
            let mut cfg2 = base.clone();
            fill(&store, &mut cfg2, &sources, &tokens, origin).await.unwrap();
            assert_eq!(cfg2.username.as_deref(), Some(user.as_str()), "{what}: one lease for the connection");
            let mut s2 = dbine_drivers::open_session(&cfg2, None).await.unwrap();
            s2.server_version().await.unwrap();
            assert!(role_exists(&user).await, "{what}: the role exists while connected");
            drop(s1);
            drop(s2);
            tokio::time::sleep(Duration::from_millis(300)).await;
            // Disconnect: the lease is revoked and Vault drops the role.
            vault_db::release(&id).await;
            let mut gone = false;
            for _ in 0..20 {
                if !role_exists(&user).await {
                    gone = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            assert!(gone, "{what}: the role is gone after the disconnect");
            eprintln!("{what}: role {user} revoked after disconnect");
            defs::forget(&[defs::digest(&id, "password", &src)]).unwrap();
        }
        // A static role: the same user, the password Vault rotates.
        let mut st = vault_db_src(&addr, "static-app", defs::VaultAuth::Token);
        if let SecretSource::VaultDb { static_role, .. } = &mut st {
            *static_role = true;
        }
        let r = fetch(&st, &[(String::new(), "dbine-root".to_string())].into()).await.unwrap();
        assert_eq!(r.username.as_deref(), Some("static_app"));
        let mut cfg = base.clone();
        cfg.username = r.username.clone();
        cfg.password = Some(r.value.clone());
        dbine_drivers::open_session(&cfg, None).await.unwrap().server_version().await.unwrap();
        eprintln!("static role: connected as static_app");
    }

    /// CyberArk Conjur Open Source (`dbine-test-conjur`, with its database
    /// `dbine-test-conjur-db`): the host `apps/dbine` reads `db/prod/password`
    /// and `db/prod/username` (`pw`, `postgres`), and DBine connects to
    /// `dbine-test-postgres` with them. Setup: docs/secret-sources.md, "For
    /// developers". Then `DBINE_TEST_CONJUR=http://127.0.0.1:28280
    /// DBINE_TEST_CONJUR_KEY=<the host's API key> cargo test -p dbine secret_sources::tests::live_conjur -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_conjur() {
        let (Ok(url), Ok(key)) = (std::env::var("DBINE_TEST_CONJUR"), std::env::var("DBINE_TEST_CONJUR_KEY")) else { return };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = temp_dir("conjur-live");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let src = |variable: &str| SecretSource::Conjur {
            url: url.clone(),
            account: "myorg".into(),
            login: "host/apps/dbine".into(),
            variable: variable.into(),
            username_variable: "db/prod/username".into(),
            json_key: String::new(),
            ca_file: String::new(),
            secret_account: String::new(),
        };
        let tokens: Secrets = [(defs::token_key("password", ""), key.clone())].into();
        let mut cfg = ConnectionConfig { driver: "postgres".into(), host: "127.0.0.1".into(), port: 25010, database: "postgres".into(), username: Some("someone-else".into()), ..Default::default() };
        let sources: SecretSources = [("password".to_string(), src("db/prod/password"))].into();
        fill(&store, &mut cfg, &sources, &tokens, Origin::Form { id: None, duplicate_of: None, name: "" }).await.unwrap();
        assert_eq!(cfg.username.as_deref(), Some("postgres"), "the user from the second variable");
        let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap();
        eprintln!("conjur: connected, {}", s.server_version().await.unwrap());
        let e = fetch(&src("db/forbidden"), &field_tokens(&tokens, "password")).await.unwrap_err();
        assert!(e.contains("permiso denegado") || e.contains("no existe"), "{e}");
        let wrong: Secrets = [(String::new(), "wrong-key".to_string())].into();
        let e = fetch(&src("db/prod/password"), &wrong).await.unwrap_err();
        assert!(e.contains("rechazó el login") && !e.contains("wrong-key"), "{e}");
    }
}

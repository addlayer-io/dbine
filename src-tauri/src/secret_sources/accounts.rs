//! Secret-manager accounts (dbine_core::secret_accounts): connecting one
//! (the "Conectar …" wizards), renaming, reconnecting and removing them,
//! and resolving a definition that names one.
//!
//! - Connecting keeps nothing until the end. Zoho Vault exchanges a code
//!   first: the result waits in memory under a pending ID until "finish"
//!   verifies the passphrase (cancelling revokes it at Zoho). Every other
//!   manager is checked live in one call at the end of its wizard
//!   ([`connect_secret_account`]) with what the wizard typed, and only a
//!   check that passed writes the keychain and the account's row.
//! - Credentials only go where the account says: Zoho's accounts servers
//!   (zoho::accounts_servers); the Bitwarden server's `/identity` and `/api`;
//!   the Vault, Secret Server (or the `*.delinea.app` tenant), Conjur or
//!   CCP URL the user typed; AWS's STS endpoint for the region; the
//!   `*.vault.azure.net` vault (the token comes from the Azure CLI); Google's
//!   APIs through gcloud; `op` for 1Password; a file for KeePass. Over HTTPS
//!   (plain HTTP only to this computer), without following redirects.
//! - A definition with an account is expanded here into the inline form,
//!   with the account's sign-in and keychain secrets, right before it runs;
//!   the confirmation of imported definitions is checked before that, on
//!   the definition as stored.
//! - Access tokens are cached by the account's credentials (zoho.rs,
//!   bitwarden.rs), so every connection using an account shares one.
//! - These commands are the connection form's and Settings' only: MCP, the
//!   assistant and the phone have no way to them.

use super::{aws, azure, bitwarden, cyberark, delinea, gcp, keepass, onepassword, vault, zoho};
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::secret_accounts::{self as accts, AccountConfig, SecretAccount};
use dbine_core::secret_sources::{self as defs, wipe, MasterSource, SecretSource, VaultAuth};
use dbine_core::secrets::Secrets;
use dbine_core::StateStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tauri::State;

/// The definition names a Zoho account this computer doesn't have (an
/// import, a restored backup, an account removed). The form shows the
/// account picker.
pub const MISSING_ACCOUNT: &str = "la cuenta de Zoho Vault de esta conexión no está en esta computadora: elegí o conectá una cuenta de Zoho Vault en la conexión";

/// The passphrase typed in the wizard didn't open the account.
pub const PASSPHRASE_MISMATCH: &str = "la frase no coincide con la frase de contraseña maestra de esta cuenta de Zoho Vault";

/// A manager's name in messages, by account kind (brand names: not translated).
pub fn provider_name(kind: &str) -> &'static str {
    match kind {
        "zoho" => "Zoho Vault",
        "bitwarden" => "Bitwarden",
        "bitwarden_sm" => "Bitwarden Secrets Manager",
        "vault" => "HashiCorp Vault",
        "delinea" => "Delinea",
        "conjur" => "CyberArk Conjur",
        "ccp" => "CyberArk CCP",
        "onepassword" => "1Password",
        "aws" => "AWS",
        "azure" => "Azure Key Vault",
        "gcp" => "Google Cloud",
        "keepass" => "KeePass",
        _ => "",
    }
}

/// The missing-account error of a definition (Zoho's keeps its words).
pub fn missing_account(account_kind: &str) -> String {
    if account_kind == "zoho" {
        return MISSING_ACCOUNT.to_string();
    }
    let p = provider_name(account_kind);
    format!("la cuenta de {p} de esta conexión no está en esta computadora: elegí o conectá una cuenta de {p} en la conexión")
}

// ---- resolving --------------------------------------------------------------------

/// Master passwords asked for an account ("ask when connecting") this run:
/// one typed for any of its connections serves the others. Never on disk.
static ASKED: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Mutex::default);

fn forget_asked(account_id: &str) {
    if let Some(mut p) = ASKED.lock().unwrap_or_else(|e| e.into_inner()).remove(account_id) {
        wipe(&mut p);
    }
}

/// A definition ready to run: the inline form of one that names an account.
pub struct Expanded {
    pub src: SecretSource,
    pub slots: Secrets,
    /// The account it came from, with its master password choice.
    pub account: Option<(String, MasterSource)>,
}

impl Drop for Expanded {
    fn drop(&mut self) {
        for v in self.slots.values_mut() {
            wipe(v);
        }
    }
}

/// `src` as it runs: unchanged, or, naming an account, inline with the
/// account's sign-in and keychain secrets. `slots`: the field's own (a
/// master password asked for the connection, in `""`).
pub fn expand(store: &StateStore, src: &SecretSource, slots: Secrets) -> Result<Expanded, String> {
    expand_with(store, src, slots, accts::get_secret)
}

/// The same, reading the account's secrets with `get` (the keychain, or a
/// map in tests).
pub fn expand_with(store: &StateStore, src: &SecretSource, mut slots: Secrets, get: impl Fn(&str, &str) -> dbine_core::Result<Option<String>>) -> Result<Expanded, String> {
    let Some(id) = src.account() else { return Ok(Expanded { src: src.clone(), slots, account: None }) };
    let want = src.account_kind().unwrap_or("");
    let account = match store.get_secret_account(id) {
        Ok(Some(a)) => a,
        Ok(None) => return Err(missing_account(want)),
        Err(e) => return Err(e.to_string()),
    };
    let config = &account.config;
    if config.kind() != want {
        return Err(format!("la cuenta «{}» de esta conexión es de otro gestor de secretos: elegí una cuenta de {} en la conexión", account.label, provider_name(want)));
    }
    let read = |slot: &str| -> Result<Option<String>, String> {
        get(id, slot).map(|v| v.filter(|v| !v.is_empty())).map_err(|e| format!("no se pudo leer la cuenta «{}» del llavero: {e}", account.label))
    };
    let missing = || format!("a la cuenta «{}» le faltan datos en el llavero: reconectala en Configuración › Gestores de secretos", account.label);
    let mut out = Secrets::new();
    // What signs in: the required slots, then the optional ones.
    let put = |out: &mut Secrets, slot: &str, as_slot: &str, required: bool| -> Result<(), String> {
        match read(slot)? {
            Some(v) => {
                out.insert(as_slot.to_string(), v);
                Ok(())
            }
            None if required => Err(missing()),
            None => Ok(()),
        }
    };
    match config {
        AccountConfig::Zoho { .. } => {
            put(&mut out, "client_secret", "client_secret", true)?;
            put(&mut out, "refresh_token", "refresh_token", true)?;
        }
        AccountConfig::Bitwarden { .. } => put(&mut out, "client_secret", "client_secret", true)?,
        AccountConfig::BitwardenSm { .. } => put(&mut out, "token", "", true)?,
        AccountConfig::Vault { auth: VaultAuth::Approle, .. } => put(&mut out, "secret_id", "secret_id", false)?,
        // No token kept: VAULT_TOKEN, ~/.vault-token (vault::token).
        AccountConfig::Vault { .. } => put(&mut out, "token", "", false)?,
        AccountConfig::Conjur { .. } => put(&mut out, "api_key", "", true)?,
        AccountConfig::Ccp { .. } => put(&mut out, "passphrase", "", false)?,
        AccountConfig::Delinea { .. } | AccountConfig::KeePass { .. } | AccountConfig::OnePassword { .. } | AccountConfig::Aws { .. } | AccountConfig::Azure { .. } | AccountConfig::Gcp { .. } => {}
    }
    // The master password (Delinea's password): kept, asked or a command's.
    let mut typed = slots.remove("").filter(|t| !t.is_empty());
    let master = config.master().map(|(_, m, _)| m).unwrap_or_default();
    if let Some((slot, m, _)) = config.master() {
        let value = match m {
            MasterSource::Keychain => {
                let v = read(slot)?;
                if v.is_none() && config.required_slots().contains(&slot) {
                    return Err(missing());
                }
                v
            }
            MasterSource::Ask => typed.take().or_else(|| ASKED.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()),
            MasterSource::Command | MasterSource::None => None,
        };
        if let Some(v) = value {
            out.insert(String::new(), v);
        }
    }
    if let Some(t) = &mut typed {
        wipe(t);
    }
    for v in slots.values_mut() {
        wipe(v);
    }
    let src = inline(src, config)?;
    Ok(Expanded { src, slots: out, account: Some((id.to_string(), master)) })
}

/// The inline definition an account definition stands for: its "what"
/// with the account's sign-in.
fn inline(src: &SecretSource, config: &AccountConfig) -> Result<SecretSource, String> {
    let mut s = src.clone();
    let mismatch = || "la cuenta no es de este gestor de secretos".to_string();
    match (&mut s, config) {
        (SecretSource::Zoho { data_center, client_id, master, master_command, account, .. }, AccountConfig::Zoho { data_center: dc, client_id: ci, master: m, master_command: mc }) => {
            *data_center = dc.clone();
            *client_id = ci.clone();
            *master = *m;
            *master_command = mc.clone();
            account.clear();
        }
        (SecretSource::Bitwarden { server, client_id, master, master_command, secret_account, .. }, AccountConfig::Bitwarden { server: sv, client_id: ci, master: m, master_command: mc }) => {
            *server = sv.clone();
            *client_id = ci.clone();
            *master = *m;
            *master_command = mc.clone();
            secret_account.clear();
        }
        (SecretSource::BitwardenSm { server, secret_account, .. }, AccountConfig::BitwardenSm { server: sv }) => {
            *server = sv.clone();
            secret_account.clear();
        }
        (
            SecretSource::Vault { address, namespace, ca_file, auth, approle_mount, role_id, secret_account, .. }
            | SecretSource::VaultDb { address, namespace, ca_file, auth, approle_mount, role_id, secret_account, .. },
            AccountConfig::Vault { address: a, namespace: n, ca_file: c, auth: au, approle_mount: am, role_id: r },
        ) => {
            *address = a.clone();
            *namespace = n.clone();
            *ca_file = c.clone();
            *auth = *au;
            *approle_mount = am.clone();
            *role_id = r.clone();
            secret_account.clear();
        }
        (SecretSource::Delinea { url, platform_url, username, domain, master, master_command, secret_account, .. }, AccountConfig::Delinea { url: u, platform_url: p, username: un, domain: d, master: m, master_command: mc }) => {
            *url = u.clone();
            *platform_url = p.clone();
            *username = un.clone();
            *domain = d.clone();
            *master = *m;
            *master_command = mc.clone();
            secret_account.clear();
        }
        (SecretSource::Conjur { url, account, login, ca_file, secret_account, .. }, AccountConfig::Conjur { url: u, conjur_account, login: l, ca_file: c }) => {
            *url = u.clone();
            *account = conjur_account.clone();
            *login = l.clone();
            *ca_file = c.clone();
            secret_account.clear();
        }
        (SecretSource::Ccp { url, app_id, client_cert, client_key, ca_file, secret_account, .. }, AccountConfig::Ccp { url: u, app_id: a, client_cert: cc, client_key: ck, ca_file: c }) => {
            *url = u.clone();
            *app_id = a.clone();
            *client_cert = cc.clone();
            *client_key = ck.clone();
            *ca_file = c.clone();
            secret_account.clear();
        }
        (SecretSource::OnePassword { account, secret_account, .. }, AccountConfig::OnePassword { account: a }) => {
            *account = a.clone();
            secret_account.clear();
        }
        (SecretSource::Aws { region, profile, secret_account, .. } | SecretSource::Ssm { region, profile, secret_account, .. }, AccountConfig::Aws { profile: p, region: r }) => {
            *region = r.clone();
            *profile = p.clone();
            secret_account.clear();
        }
        (SecretSource::Azure { vault_url, tenant, secret_account, .. }, AccountConfig::Azure { tenant: t, vault_url: v }) => {
            *vault_url = v.clone();
            *tenant = t.clone();
            secret_account.clear();
        }
        (SecretSource::Gcp { project, secret_account, .. }, AccountConfig::Gcp { project: p }) => {
            *project = p.clone();
            secret_account.clear();
        }
        (SecretSource::KeePass { file, key_file, master, master_command, secret_account, .. }, AccountConfig::KeePass { file: f, key_file: k, master: m, master_command: mc }) => {
            *file = f.clone();
            *key_file = k.clone();
            *master = *m;
            *master_command = mc.clone();
            secret_account.clear();
        }
        _ => return Err(mismatch()),
    }
    Ok(s)
}

/// The messages that say an asked master password was wrong.
fn wrong_master(message: &str) -> bool {
    [zoho::WRONG_PASSPHRASE, bitwarden::WRONG_PASSWORD, delinea::WRONG_PASSWORD, keepass::WRONG_KEY].contains(&message)
}

/// After a run through an account: an asked master password that worked
/// is kept for its other connections; one that didn't is dropped.
pub fn after_run(e: &Expanded, ok: Result<(), &str>) {
    let Some((id, MasterSource::Ask)) = &e.account else { return };
    match ok {
        Ok(()) => {
            if let Some(p) = e.slots.get("") {
                ASKED.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), p.clone());
            }
        }
        Err(m) if wrong_master(m) => forget_asked(id),
        Err(_) => {}
    }
}

/// The access token kept for an account (removed, reconnected) is dropped.
fn forget_account_token(a: &SecretAccount) {
    let secret = |slot: &str| accts::get_secret(&a.id, slot).ok().flatten().unwrap_or_default();
    match &a.config {
        AccountConfig::Zoho { data_center, client_id, .. } => {
            let creds = zoho::Creds { client_id: client_id.clone(), client_secret: secret("client_secret"), refresh_token: secret("refresh_token"), passphrase: String::new() };
            zoho::forget_token_of(data_center, &creds);
        }
        AccountConfig::Bitwarden { server, client_id, .. } => {
            let mut s = secret("client_secret");
            bitwarden::forget_token_of(server, client_id, &s);
            wipe(&mut s);
        }
        AccountConfig::BitwardenSm { server } => {
            let mut t = secret("token");
            bitwarden::sm_forget(server, &t);
            wipe(&mut t);
        }
        _ => {}
    }
}

// ---- connecting Zoho Vault (its own wizard) -----------------------------------------

/// A connect between the exchange and "finish": in memory, for a while.
struct Pending {
    data_center: String,
    client_id: String,
    client_secret: String,
    exchanged: zoho::Exchanged,
    /// `GET_LOGIN`'s salt, iterations and check value.
    check: Value,
    identity: String,
    at: Instant,
}

impl Drop for Pending {
    fn drop(&mut self) {
        wipe(&mut self.client_secret);
    }
}

/// How long a connect may wait between the exchange and "finish".
const PENDING_TTL: Duration = Duration::from_secs(30 * 60);

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(Mutex::default);

fn pending_take(id: &str) -> Option<Pending> {
    let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    p.retain(|_, v| v.at.elapsed() < PENDING_TTL);
    p.remove(id)
}

#[derive(Deserialize)]
pub struct ZohoExchangeArgs {
    pub client_id: String,
    pub client_secret: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct ZohoExchanged {
    /// What "finish" and "cancel" name.
    pub pending: String,
    pub data_center: String,
    /// The user's e-mail or name, for the label ('' if Zoho didn't say).
    pub identity: String,
}

/// Step 3: exchange the code, find the data centre, read the user.
#[tauri::command(rename_all = "camelCase")]
pub async fn zoho_connect_exchange(args: ZohoExchangeArgs) -> CommandResult<ZohoExchanged> {
    if !accts::valid_zoho_client_id(&args.client_id) {
        return Err(CommandError::BadRequest("el ID del cliente de Zoho no es válido (1000.XXXX)".into()));
    }
    for v in [&args.client_secret, &args.code] {
        if v.trim().is_empty() || v.len() > 1000 || v.chars().any(char::is_control) {
            return Err(CommandError::BadRequest("faltan el secreto del cliente o el código de Zoho".into()));
        }
    }
    let client = zoho::https_client().map_err(CommandError::SecretSource)?;
    let ex = zoho::exchange_code(&args.client_id, &args.client_secret, &args.code).await.map_err(CommandError::SecretSource)?;
    let (accounts, vault) = match defs::zoho_hosts(&ex.data_center) {
        Some(h) => h,
        None => {
            let host = zoho::ACCOUNTS_WITHOUT_VAULT.iter().find(|(id, _)| *id == ex.data_center).map(|(_, h)| *h).unwrap_or("");
            revoke_later(format!("https://{host}"), args.client_id.trim().to_string(), args.client_secret.trim().to_string(), ex.refresh_token.clone());
            return Err(CommandError::SecretSource(format!("tu cuenta de Zoho está en un centro de datos ({host}) donde DBine no conoce la dirección de Zoho Vault")));
        }
    };
    let details = match zoho::login_details_with(&client, &format!("https://{vault}"), &ex.access_token).await {
        Ok(d) => d,
        Err(e) => {
            revoke_later(format!("https://{accounts}"), args.client_id.trim().to_string(), args.client_secret.trim().to_string(), ex.refresh_token.clone());
            return Err(CommandError::SecretSource(e));
        }
    };
    Ok(stash(args.client_id.trim(), args.client_secret.trim(), ex, &details))
}

/// Keep an exchange's result for "finish".
fn stash(client_id: &str, client_secret: &str, ex: zoho::Exchanged, details: &Value) -> ZohoExchanged {
    let id = uuid::Uuid::new_v4().to_string();
    let out = ZohoExchanged { pending: id.clone(), data_center: ex.data_center.clone(), identity: zoho::identity_of(details) };
    let p = Pending {
        data_center: ex.data_center.clone(),
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        exchanged: ex,
        check: zoho::passphrase_check(details),
        identity: out.identity.clone(),
        at: Instant::now(),
    };
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(id, p);
    out
}

fn revoke_later(accounts_base: String, client_id: String, mut client_secret: String, mut refresh_token: String) {
    tauri::async_runtime::spawn(async move {
        if let Ok(c) = zoho::https_client() {
            if let Err(e) = zoho::revoke_with(&c, &accounts_base, &client_id, &client_secret, &refresh_token).await {
                tracing::debug!("zoho revoke: {e}");
            }
        }
        wipe(&mut client_secret);
        wipe(&mut refresh_token);
    });
}

#[derive(Deserialize)]
pub struct ZohoFinishArgs {
    pub pending: String,
    /// Reconnecting: the account kept (its ID, so its connections go on).
    #[serde(default)]
    pub account_id: Option<String>,
    pub label: String,
    pub master: MasterSource,
    #[serde(default)]
    pub master_command: String,
    /// Typed in the wizard (keychain and ask; ask only checks it).
    #[serde(default)]
    pub passphrase: String,
}

/// What "finish" writes: the account's row and its keychain items.
struct Finished {
    account: SecretAccount,
    secrets: Vec<(&'static str, String)>,
}

impl Drop for Finished {
    fn drop(&mut self) {
        for (_, v) in &mut self.secrets {
            wipe(v);
        }
    }
}

/// The account a pending connect becomes, once its passphrase opens it.
/// `passphrase`: typed, or the command's output.
fn finish_account(p: &Pending, args: &ZohoFinishArgs, existing: Option<&SecretAccount>, passphrase: &str) -> Result<Finished, String> {
    let mut key = zoho::master_key(&p.check, passphrase).map_err(|e| if e == zoho::WRONG_PASSPHRASE { PASSPHRASE_MISMATCH.to_string() } else { e })?;
    wipe(&mut key);
    let ts = existing.map(|a| a.created_at.clone()).unwrap_or_default();
    let account = SecretAccount {
        id: existing.map(|a| a.id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        label: args.label.trim().to_string(),
        identity: p.identity.clone(),
        config: AccountConfig::Zoho { data_center: p.data_center.clone(), client_id: p.client_id.clone(), master: args.master, master_command: if args.master == MasterSource::Command { args.master_command.trim().to_string() } else { String::new() } },
        created_at: ts.clone(),
        updated_at: ts,
    };
    account.validate().map_err(|e| e.to_string())?;
    let mut secrets = vec![("client_secret", p.client_secret.clone()), ("refresh_token", p.exchanged.refresh_token.clone())];
    if args.master == MasterSource::Keychain {
        secrets.push(("passphrase", passphrase.to_string()));
    }
    Ok(Finished { account, secrets })
}

/// Step 4: check the passphrase, then save the account (or replace a
/// reconnected one's sign-in, keeping its ID).
#[tauri::command(rename_all = "camelCase")]
pub async fn zoho_connect_finish(state: State<'_, AppState>, args: ZohoFinishArgs) -> CommandResult<SecretAccount> {
    accts::check_label(&args.label).map_err(|e| CommandError::BadRequest(e.to_string()))?;
    let existing = match args.account_id.as_deref().filter(|i| !i.is_empty()) {
        Some(id) => Some(state.store.get_secret_account(id)?.ok_or_else(|| CommandError::NotFound(MISSING_ACCOUNT.into()))?),
        None => None,
    };
    let Some(p) = pending_take(&args.pending) else {
        return Err(CommandError::BadRequest("la conexión con Zoho expiró: volvé a generar el código".into()));
    };
    let mut passphrase = match args.master {
        MasterSource::Command => {
            if args.master_command.trim().is_empty() || args.master_command.contains(['\n', '\r', '\0']) {
                put_back(&args.pending, p);
                return Err(CommandError::BadRequest("falta el comando que devuelve la contraseña maestra".into()));
            }
            match super::exec::run(super::exec::shell_command(&args.master_command), super::exec::COMMAND_LIMIT).await {
                Ok(out) => super::exec::trim_newline(out),
                Err(f) => {
                    put_back(&args.pending, p);
                    return Err(CommandError::SecretSource(format!("la contraseña maestra: {}", super::command_failure(f))));
                }
            }
        }
        _ => args.passphrase.clone(),
    };
    let done = finish_account(&p, &args, existing.as_ref(), &passphrase);
    wipe(&mut passphrase);
    let done = match done {
        Ok(d) => d,
        Err(e) => {
            // A wrong passphrase: the user tries again with the same code.
            put_back(&args.pending, p);
            return Err(CommandError::SecretSource(e));
        }
    };
    let id = done.account.id.clone();
    if let Some(old) = &existing {
        forget_account_token(old);
        forget_asked(&old.id);
    }
    // The keychain first: a row never points to missing secrets.
    let write = || -> dbine_core::Result<()> {
        for (slot, v) in &done.secrets {
            accts::set_secret(&id, slot, v)?;
        }
        for slot in accts::ALL_SLOTS {
            if !done.secrets.iter().any(|(s, _)| s == slot) {
                accts::delete_secret(&id, slot)?;
            }
        }
        Ok(())
    };
    if let Err(e) = write() {
        if existing.is_none() {
            let _ = accts::delete_secrets(&id);
        }
        return Err(e.into());
    }
    let saved = match state.store.save_secret_account(&done.account) {
        Ok(a) => a,
        Err(e) => {
            if existing.is_none() {
                let _ = accts::delete_secrets(&id);
            }
            return Err(e.into());
        }
    };
    // The access token just issued serves the first use.
    let creds = zoho::Creds { client_id: p.client_id.clone(), client_secret: p.client_secret.clone(), refresh_token: p.exchanged.refresh_token.clone(), passphrase: String::new() };
    zoho::keep_first_token(&p.data_center, &creds, &p.exchanged);
    super::forget_all_cached();
    Ok(saved)
}

fn put_back(id: &str, mut p: Pending) {
    p.at = Instant::now();
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_string(), p);
}

#[derive(Deserialize)]
pub struct ZohoCancelArgs {
    pub pending: String,
}

/// The wizard closed before finishing: nothing was saved; the refresh
/// token the code gave is revoked.
#[tauri::command(rename_all = "camelCase")]
pub async fn zoho_connect_cancel(args: ZohoCancelArgs) -> CommandResult<()> {
    if let Some(p) = pending_take(&args.pending) {
        if let Some((accounts, _)) = defs::zoho_hosts(&p.data_center) {
            revoke_later(format!("https://{accounts}"), p.client_id.clone(), p.client_secret.clone(), p.exchanged.refresh_token.clone());
        }
    }
    Ok(())
}

/// Where the Self Client is created.
pub const ZOHO_API_CONSOLE: &str = "https://api-console.zoho.com/";

/// Step 1: open Zoho's API console in the browser (that address only).
#[tauri::command(rename_all = "camelCase")]
pub async fn open_zoho_api_console(app: tauri::AppHandle) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(ZOHO_API_CONSOLE, None::<&str>)
        .map_err(|e| CommandError::Internal(format!("no se pudo abrir el navegador: {e}")))
}

// ---- connecting every other manager (one check at the end) -----------------------------

/// The secrets a wizard typed, by slot; wiped when dropped.
#[derive(Default, Deserialize)]
#[serde(transparent)]
pub struct Typed(pub HashMap<String, String>);

impl Drop for Typed {
    fn drop(&mut self) {
        for v in self.0.values_mut() {
            wipe(v);
        }
    }
}

impl std::fmt::Debug for Typed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

#[derive(Deserialize)]
pub struct ConnectAccountArgs {
    /// Reconnecting: the account kept (its ID, so its connections go on).
    #[serde(default)]
    pub account_id: Option<String>,
    /// Empty: the manager and who signed in ("Bitwarden · ana@example.com").
    #[serde(default)]
    pub label: String,
    pub config: AccountConfig,
    /// What the wizard typed, by slot (`client_secret`, `passphrase`,
    /// `token`, `secret_id`, `password`, `api_key`). A master password
    /// asked when connecting is checked here and never kept.
    #[serde(default)]
    pub secrets: Typed,
}

#[derive(Debug, Serialize)]
pub struct ConnectedAccount {
    pub account: SecretAccount,
    /// What the check found besides who signed in ("políticas: …",
    /// "12 entradas"), a backend message ('' when there's nothing to add).
    pub detail: String,
}

/// Where an account's credentials go: when it changes on reconnecting, the
/// secrets kept for the old one aren't reused (they're typed again).
fn endpoint(c: &AccountConfig) -> String {
    match c {
        AccountConfig::Zoho { data_center, client_id, .. } => format!("{data_center} {client_id}"),
        AccountConfig::Bitwarden { server, client_id, .. } => format!("{} {}", defs::bitwarden_urls(server).map(|(i, _)| i).unwrap_or_default(), client_id.trim()),
        AccountConfig::BitwardenSm { server } => defs::bitwarden_urls(server).map(|(i, _)| i).unwrap_or_default(),
        AccountConfig::Vault { address, namespace, auth, approle_mount, role_id, .. } => format!("{} {namespace} {auth:?} {approle_mount} {role_id}", address.trim().trim_end_matches('/')),
        AccountConfig::Delinea { url, platform_url, username, domain, .. } => format!("{} {} {username} {domain}", url.trim().trim_end_matches('/'), platform_url.trim()),
        AccountConfig::Conjur { url, conjur_account, login, .. } => format!("{} {conjur_account} {login}", url.trim().trim_end_matches('/')),
        AccountConfig::Ccp { url, client_cert, client_key, .. } => format!("{} {client_cert} {client_key}", url.trim().trim_end_matches('/')),
        AccountConfig::KeePass { file, key_file, .. } => format!("{file} {key_file}"),
        AccountConfig::OnePassword { .. } | AccountConfig::Aws { .. } | AccountConfig::Azure { .. } | AccountConfig::Gcp { .. } => String::new(),
    }
}

/// What the check signs in with: the slots typed now, else (reconnecting
/// to the same endpoint) the ones kept. `get` reads the keychain.
fn gather(config: &AccountConfig, typed: &Typed, existing: Option<&SecretAccount>, get: impl Fn(&str, &str) -> dbine_core::Result<Option<String>>) -> CommandResult<Secrets> {
    let mut out = Secrets::new();
    let master_slot = config.master().map(|(slot, _, _)| slot);
    for (slot, v) in &typed.0 {
        let known = accts::ALL_SLOTS.contains(&slot.as_str());
        if known && !v.is_empty() && (config.slots().contains(&slot.as_str()) || master_slot == Some(slot.as_str())) {
            out.insert(slot.clone(), v.clone());
        }
    }
    if let Some(old) = existing.filter(|o| o.config.kind() == config.kind() && endpoint(&o.config) == endpoint(config)) {
        for slot in config.slots() {
            if !out.contains_key(*slot) {
                if let Some(v) = get(&old.id, slot)?.filter(|v| !v.is_empty()) {
                    out.insert(slot.to_string(), v);
                }
            }
        }
    }
    Ok(out)
}

/// The check, live, against the manager: who signed in, and what else it
/// found. Each sends the credentials only to the account's own endpoint.
async fn verify(config: &AccountConfig, s: &Secrets) -> Result<(String, String), String> {
    let slot = |k: &str| s.get(k).cloned().unwrap_or_default();
    let missing = |what: &str| format!("falta {what}");
    match config {
        AccountConfig::Zoho { .. } => Err("Zoho Vault se conecta con su propio asistente".into()),
        AccountConfig::Bitwarden { server, client_id, .. } => {
            let creds = bitwarden::Creds { client_id: client_id.trim().to_string(), client_secret: slot("client_secret"), password: slot("passphrase") };
            if creds.client_secret.trim().is_empty() {
                return Err(missing("el client_secret de la clave de API"));
            }
            if creds.password.is_empty() {
                return Err(missing("la contraseña maestra de Bitwarden"));
            }
            Ok((bitwarden::whoami(server, &creds).await?, String::new()))
        }
        AccountConfig::BitwardenSm { server } => {
            let mut token = slot("token");
            if token.trim().is_empty() {
                return Err(missing("el token de acceso de la cuenta de máquina"));
            }
            let r = bitwarden::sm_whoami(server, &token).await;
            wipe(&mut token);
            let (org, n) = r?;
            Ok((org, format!("{n} secretos visibles para la cuenta de máquina")))
        }
        AccountConfig::Vault { address, namespace, ca_file, auth, approle_mount, role_id } => {
            let conn = vault::Conn { address, namespace };
            // As vault::Auth reads a field's slots: the token in "".
            let mut vs: Secrets = [("", "token"), ("secret_id", "secret_id")].iter().filter_map(|(to, from)| s.get(*from).filter(|v| !v.is_empty()).map(|v| (to.to_string(), v.clone()))).collect();
            let auth = vault::Auth::of(*auth, approle_mount, role_id, &vs);
            let r = vault::whoami(&conn, ca_file, &auth).await;
            for v in vs.values_mut() {
                wipe(v);
            }
            let (name, policies) = r?;
            let detail = if policies.is_empty() { String::new() } else { format!("políticas: {}", policies.join(", ")) };
            Ok((name, detail))
        }
        AccountConfig::Delinea { url, platform_url, username, domain, .. } => {
            let mut password = slot("password");
            if password.is_empty() {
                return Err(missing("la contraseña de Delinea"));
            }
            let req = delinea::Request { url, platform_url, username, domain, secret: "", field: "", fill_username: false, user_field: "" };
            let r = delinea::whoami(&req, &password).await;
            wipe(&mut password);
            Ok((r?, String::new()))
        }
        AccountConfig::Conjur { url, conjur_account, login, ca_file } => {
            let mut key = slot("api_key");
            let req = cyberark::Conjur { url, account: conjur_account, login, variable: "", username_variable: "", ca_file };
            let r = cyberark::conjur_whoami(&req, &key).await;
            wipe(&mut key);
            Ok((r?, String::new()))
        }
        AccountConfig::Ccp { url, app_id, client_cert, client_key, ca_file } => {
            let mut pass = slot("passphrase");
            let r = cyberark::ccp_handshake(url, client_cert, client_key, ca_file, Some(pass.as_str()).filter(|p| !p.is_empty())).await;
            wipe(&mut pass);
            let status = r?;
            let detail = if client_cert.trim().is_empty() {
                format!("conexión TLS aceptada (HTTP {status}), sin certificado de cliente; el AppID se prueba con «Probar» en una conexión")
            } else {
                format!("conexión TLS con el certificado de cliente aceptada (HTTP {status}); el AppID se prueba con «Probar» en una conexión")
            };
            Ok((app_id.trim().to_string(), detail))
        }
        AccountConfig::OnePassword { account } => Ok((onepassword::whoami(account).await?, String::new())),
        AccountConfig::Aws { profile, region } => Ok((aws::caller_identity(profile, region).await?, String::new())),
        AccountConfig::Azure { tenant, vault_url } => {
            let (who, tid) = azure::whoami(vault_url, tenant).await?;
            Ok((who, if tid.is_empty() { String::new() } else { format!("inquilino {tid}") }))
        }
        AccountConfig::Gcp { .. } => Ok((gcp::whoami().await?, String::new())),
        AccountConfig::KeePass { file, key_file, .. } => {
            let master = s.get("passphrase").filter(|p| !p.is_empty()).cloned();
            let key_file = Some(key_file.trim()).filter(|k| !k.is_empty()).map(super::expand_home);
            let n = keepass::verify(super::expand_home(file.trim()), key_file, master).await?;
            let name = std::path::Path::new(file.trim()).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| file.trim().to_string());
            Ok((name, format!("{n} entradas")))
        }
    }
}

/// The host of a URL, for a label.
fn host_of(url: &str) -> String {
    let rest = url.trim().split_once("://").map_or(url.trim(), |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// The name an account gets when the wizard leaves it empty.
pub fn default_label(config: &AccountConfig, identity: &str) -> String {
    let p = provider_name(config.kind());
    let who = match config {
        AccountConfig::BitwardenSm { server } => defs::bitwarden_urls(server).map(|(i, _)| host_of(&i).trim_start_matches("identity.").to_string()).unwrap_or_default(),
        AccountConfig::Vault { address, .. } => host_of(address),
        AccountConfig::Ccp { app_id, url, .. } => format!("{} · {}", app_id.trim(), host_of(url)),
        AccountConfig::Aws { profile, region } => format!("{} · {}", if profile.is_empty() { "default" } else { profile.as_str() }, region.trim()),
        AccountConfig::Azure { vault_url, .. } => host_of(vault_url).split('.').next().unwrap_or("").to_string(),
        AccountConfig::Gcp { project } => project.trim().to_string(),
        AccountConfig::KeePass { .. } | AccountConfig::Bitwarden { .. } | AccountConfig::Delinea { .. } | AccountConfig::Conjur { .. } | AccountConfig::OnePassword { .. } | AccountConfig::Zoho { .. } => identity.to_string(),
    };
    let who: String = who.chars().filter(|c| !c.is_control()).take(150).collect();
    if who.trim().is_empty() { p.to_string() } else { format!("{p} · {}", who.trim()) }
}

/// The account a check that passed becomes, and what goes to the keychain:
/// only the slots the account keeps (an asked master password never).
/// An account and the keychain items to write for it.
type Saving = (SecretAccount, Vec<(&'static str, String)>);

fn finish_generic(config: &AccountConfig, label: &str, identity: &str, existing: Option<&SecretAccount>, checked: &Secrets) -> Result<Saving, String> {
    let ts = existing.map(|a| a.created_at.clone()).unwrap_or_default();
    let identity: String = identity.chars().filter(|c| !c.is_control()).take(320).collect();
    let label = if label.trim().is_empty() { default_label(config, &identity) } else { label.trim().to_string() };
    let account = SecretAccount {
        id: existing.map(|a| a.id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        label,
        identity,
        config: config.clone(),
        created_at: ts.clone(),
        updated_at: ts,
    };
    account.validate().map_err(|e| e.to_string())?;
    let keep = config.slots().iter().filter_map(|slot| checked.get(*slot).filter(|v| !v.is_empty()).map(|v| (*slot, v.clone()))).collect();
    Ok((account, keep))
}

/// The last step of every wizard but Zoho's: check the account live and,
/// only if that passed, save it (reconnecting: replace its sign-in, keeping
/// its ID). Nothing is kept when the check fails.
#[tauri::command(rename_all = "camelCase")]
pub async fn connect_secret_account(state: State<'_, AppState>, args: ConnectAccountArgs) -> CommandResult<ConnectedAccount> {
    if args.config.kind() == "zoho" {
        return Err(CommandError::BadRequest("Zoho Vault se conecta con su propio asistente".into()));
    }
    args.config.validate().map_err(|e| CommandError::BadRequest(e.to_string()))?;
    if !args.label.trim().is_empty() {
        accts::check_label(&args.label).map_err(|e| CommandError::BadRequest(e.to_string()))?;
    }
    let existing = match args.account_id.as_deref().filter(|i| !i.is_empty()) {
        Some(id) => {
            let a = state.store.get_secret_account(id)?.ok_or_else(|| CommandError::NotFound(missing_account(args.config.kind())))?;
            if a.config.kind() != args.config.kind() {
                return Err(CommandError::BadRequest("la cuenta es de otro gestor de secretos".into()));
            }
            Some(a)
        }
        None => None,
    };
    let mut checked = gather(&args.config, &args.secrets, existing.as_ref(), accts::get_secret)?;
    // A master password from a command: run it now (and each time it's needed).
    if let Some((slot, MasterSource::Command, command)) = args.config.master() {
        match super::exec::run(super::exec::shell_command(command), super::exec::COMMAND_LIMIT).await {
            Ok(out) => {
                checked.insert(slot.to_string(), super::exec::trim_newline(out));
            }
            Err(f) => return Err(CommandError::SecretSource(format!("la contraseña maestra: {}", super::command_failure(f)))),
        }
    }
    let verified = verify(&args.config, &checked).await;
    let wipe_all = |c: &mut Secrets| {
        for v in c.values_mut() {
            wipe(v);
        }
    };
    let (identity, detail) = match verified {
        Ok(v) => v,
        Err(e) => {
            wipe_all(&mut checked);
            return Err(CommandError::SecretSource(e));
        }
    };
    let finished = finish_generic(&args.config, &args.label, &identity, existing.as_ref(), &checked);
    wipe_all(&mut checked);
    let (account, mut keep) = finished.map_err(CommandError::SecretSource)?;
    let id = account.id.clone();
    if let Some(old) = &existing {
        forget_account_token(old);
        forget_asked(&old.id);
    }
    // The keychain first: a row never points to missing secrets.
    let write = || -> dbine_core::Result<()> {
        for (slot, v) in &keep {
            accts::set_secret(&id, slot, v)?;
        }
        for slot in accts::ALL_SLOTS {
            if !keep.iter().any(|(s, _)| s == slot) {
                accts::delete_secret(&id, slot)?;
            }
        }
        Ok(())
    };
    let written = write();
    for (_, v) in &mut keep {
        wipe(v);
    }
    if let Err(e) = written {
        if existing.is_none() {
            let _ = accts::delete_secrets(&id);
        }
        return Err(e.into());
    }
    let saved = match state.store.save_secret_account(&account) {
        Ok(a) => a,
        Err(e) => {
            if existing.is_none() {
                let _ = accts::delete_secrets(&id);
            }
            return Err(e.into());
        }
    };
    super::forget_all_cached();
    Ok(ConnectedAccount { account: saved, detail })
}

#[derive(Deserialize)]
pub struct DetectAccountArgs {
    pub kind: String,
}

/// What a wizard can start from, found on this computer (never a secret):
/// 1Password's CLI and its accounts, AWS's profiles, `az account show`,
/// `gcloud config list`.
#[tauri::command(rename_all = "camelCase")]
pub async fn detect_secret_account(args: DetectAccountArgs) -> CommandResult<Value> {
    let err = |e: String| serde_json::json!({ "error": e });
    Ok(match args.kind.as_str() {
        "onepassword" => {
            if !onepassword::installed() {
                serde_json::json!({ "installed": false })
            } else {
                match onepassword::accounts().await {
                    Ok(list) => serde_json::json!({ "installed": true, "accounts": list }),
                    Err(e) => serde_json::json!({ "installed": true, "accounts": [], "error": e }),
                }
            }
        }
        "aws" => {
            let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
            let profiles: Vec<Value> = aws::profiles().into_iter().map(|(name, region)| serde_json::json!({ "name": name, "region": region })).collect();
            let region = env("AWS_REGION").or_else(|| env("AWS_DEFAULT_REGION")).filter(|r| defs::valid_aws_region(r)).unwrap_or_default();
            serde_json::json!({ "profiles": profiles, "region": region, "env_profile": env("AWS_PROFILE").unwrap_or_default(), "env_keys": env("AWS_ACCESS_KEY_ID").is_some() })
        }
        "azure" => match azure::current_account().await {
            Ok(v) => v,
            Err(e) => err(e),
        },
        "gcp" => match gcp::current_config().await {
            Ok(v) => v,
            Err(e) => err(e),
        },
        _ => return Err(CommandError::BadRequest("este gestor de secretos no tiene nada que detectar".into())),
    })
}

/// The pages a wizard opens in the browser: fixed addresses only.
pub const HELP_PAGES: &[(&str, &str)] = &[
    ("bitwarden.com", "https://vault.bitwarden.com/"),
    ("bitwarden.eu", "https://vault.bitwarden.eu/"),
    ("onepassword-cli", "https://developer.1password.com/docs/cli/get-started/"),
    ("onepassword-app", "https://developer.1password.com/docs/cli/app-integration/"),
];

#[derive(Deserialize)]
pub struct OpenHelpArgs {
    pub page: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn open_secret_manager_page(app: tauri::AppHandle, args: OpenHelpArgs) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    let url = HELP_PAGES.iter().find(|(k, _)| *k == args.page).map(|(_, u)| *u).ok_or_else(|| CommandError::BadRequest("página desconocida".into()))?;
    app.opener().open_url(url, None::<&str>).map_err(|e| CommandError::Internal(format!("no se pudo abrir el navegador: {e}")))
}

// ---- managing ----------------------------------------------------------------------

#[tauri::command(rename_all = "camelCase")]
pub async fn list_secret_accounts(state: State<'_, AppState>) -> CommandResult<Vec<SecretAccount>> {
    Ok(state.store.list_secret_accounts()?)
}

#[derive(Deserialize)]
pub struct RenameAccountArgs {
    pub id: String,
    pub label: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn rename_secret_account(state: State<'_, AppState>, args: RenameAccountArgs) -> CommandResult<SecretAccount> {
    accts::check_label(&args.label).map_err(|e| CommandError::BadRequest(e.to_string()))?;
    Ok(state.store.rename_secret_account(&args.id, &args.label)?)
}

#[derive(Deserialize)]
pub struct DeleteAccountArgs {
    pub id: String,
}

/// Remove an account: its keychain items, its cached access token, an asked
/// passphrase, its row. Connections naming it then ask for an account.
#[tauri::command(rename_all = "camelCase")]
pub async fn delete_secret_account(state: State<'_, AppState>, args: DeleteAccountArgs) -> CommandResult<()> {
    remove_account(&state.store, &args.id)?;
    super::forget_all_cached();
    Ok(())
}

fn remove_account(store: &StateStore, id: &str) -> CommandResult<()> {
    if !accts::valid_account_id(id) {
        return Err(CommandError::BadRequest("el ID de la cuenta no es válido".into()));
    }
    if let Some(a) = store.get_secret_account(id)? {
        forget_account_token(&a);
    }
    forget_asked(id);
    accts::delete_secrets(id)?;
    store.delete_secret_account(id)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(v: &Value) -> Value {
        serde_json::json!({ "LOGIN": "PBKDF2_AES", "SALT": v["salt"], "ITERATION": v["iterations"], "PASSPHRASE": v["passphrase_check"],
            "USER": { "USERID": "1", "USERNAME": "ana", "EMAIL": "ana@example.com" } })
    }

    fn vectors() -> Value {
        serde_json::from_str(include_str!("testdata/zoho-vectors.json")).unwrap()
    }

    fn pending(v: &Value) -> Pending {
        Pending {
            data_center: "eu".into(),
            client_id: "1000.CLIENT".into(),
            client_secret: "dummy-client-secret".into(),
            exchanged: zoho::Exchanged { data_center: "eu".into(), refresh_token: "1000.dummy-refresh".into(), access_token: "at-1".into(), expires_in: 3600 },
            check: zoho::passphrase_check(&details(v)),
            identity: zoho::identity_of(&details(v)),
            at: Instant::now(),
        }
    }

    fn finish_args(master: MasterSource, passphrase: &str) -> ZohoFinishArgs {
        ZohoFinishArgs { pending: "p".into(), account_id: None, label: "Zoho · ana@example.com".into(), master, master_command: String::new(), passphrase: passphrase.into() }
    }

    /// Step 4: the passphrase opens the account or says it doesn't; what's
    /// kept: the row's metadata, and the secrets for the keychain (the
    /// passphrase only when the user chose the keychain).
    #[test]
    fn finishing_checks_the_passphrase() {
        let v = vectors();
        let p = pending(&v);
        assert_eq!(p.identity, "ana@example.com");
        assert!(p.check.get("USER").is_none(), "only what checks the passphrase");
        let pass = v["passphrase"].as_str().unwrap();
        let e = finish_account(&p, &finish_args(MasterSource::Keychain, "wrong"), None, "wrong").err().unwrap();
        assert_eq!(e, PASSPHRASE_MISMATCH);
        let f = finish_account(&p, &finish_args(MasterSource::Keychain, pass), None, pass).unwrap();
        assert_eq!(f.account.config, AccountConfig::Zoho { data_center: "eu".into(), client_id: "1000.CLIENT".into(), master: MasterSource::Keychain, master_command: String::new() });
        assert_eq!(f.secrets.iter().map(|(s, _)| *s).collect::<Vec<_>>(), ["client_secret", "refresh_token", "passphrase"]);
        let row = serde_json::to_string(&f.account).unwrap();
        assert!(!row.contains("dummy-client-secret") && !row.contains("dummy-refresh") && !row.contains(pass), "{row}");
        let f = finish_account(&p, &finish_args(MasterSource::Ask, pass), None, pass).unwrap();
        assert_eq!(f.secrets.len(), 2, "an asked passphrase is checked, never kept");
        // Reconnecting keeps the ID and the creation time.
        let existing = SecretAccount { created_at: "2026-01-01T00:00:00Z".into(), ..f.account.clone() };
        let again = finish_account(&p, &finish_args(MasterSource::Keychain, pass), Some(&existing), pass).unwrap();
        assert_eq!((again.account.id.as_str(), again.account.created_at.as_str()), (existing.id.as_str(), "2026-01-01T00:00:00Z"));
    }

    /// A definition naming an account this computer doesn't have: the
    /// missing-account error (the form shows the account picker).
    #[test]
    fn a_missing_account_says_so() {
        let store = StateStore::open_in_memory().unwrap();
        let src: SecretSource = serde_json::from_str(r#"{"kind":"zoho","secret_name":"pg","account":"6f1c2d4e-0000-4000-8000-00000000dead"}"#).unwrap();
        let e = expand(&store, &src, Secrets::new()).err().unwrap();
        assert_eq!(e, MISSING_ACCOUNT);
        // Inline definitions pass through as they are.
        let inline: SecretSource = serde_json::from_str(r#"{"kind":"zoho","data_center":"eu","secret_name":"pg","client_id":"1000.A"}"#).unwrap();
        let slots: Secrets = [("client_secret".to_string(), "cs".to_string())].into();
        let x = expand(&store, &inline, slots.clone()).unwrap();
        assert_eq!((&x.src, &x.slots, x.account.is_none()), (&inline, &slots, true));
    }

    fn account(id: &str, master: MasterSource) -> SecretAccount {
        SecretAccount {
            id: id.into(),
            label: "Zoho".into(),
            identity: String::new(),
            config: AccountConfig::Zoho { data_center: "eu".into(), client_id: "1000.CLIENT".into(), master, master_command: String::new() },
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn keychain(id: &str) -> impl Fn(&str, &str) -> dbine_core::Result<Option<String>> {
        let id = id.to_string();
        move |a: &str, slot: &str| {
            Ok((a == id).then(|| match slot {
                "client_secret" => "dummy-client-secret".to_string(),
                "refresh_token" => "1000.dummy-refresh".to_string(),
                _ => "dummy-passphrase".to_string(),
            }))
        }
    }

    /// A definition naming an account runs as the inline one with the
    /// account's data centre, client and keychain secrets; two connections
    /// naming it get the same credentials (one access token for both).
    #[test]
    fn an_account_definition_resolves_through_the_account() {
        let store = StateStore::open_in_memory().unwrap();
        let id = "6f1c2d4e-0000-4000-8000-000000000001";
        store.save_secret_account(&account(id, MasterSource::Keychain)).unwrap();
        let src: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"zoho","secret_name":"pg-main","attribute":"pw","fill_username":true,"account":"{id}"}}"#)).unwrap();
        let x = expand_with(&store, &src, Secrets::new(), keychain(id)).unwrap();
        let SecretSource::Zoho { data_center, client_id, secret_name, attribute, fill_username, account: acct, master, .. } = &x.src else { panic!() };
        assert_eq!((data_center.as_str(), client_id.as_str(), secret_name.as_str(), attribute.as_str(), *fill_username, acct.as_str(), *master), ("eu", "1000.CLIENT", "pg-main", "pw", true, "", MasterSource::Keychain));
        x.src.validate().unwrap();
        assert_eq!(x.slots.get("client_secret").map(String::as_str), Some("dummy-client-secret"));
        assert_eq!(x.slots.get("").map(String::as_str), Some("dummy-passphrase"));
        assert_eq!(x.account.as_ref().map(|(a, _)| a.as_str()), Some(id));
        // Another connection, another secret: the same account credentials.
        let other: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"zoho","secret_name":"mysql","account":"{id}"}}"#)).unwrap();
        let y = expand_with(&store, &other, Secrets::new(), keychain(id)).unwrap();
        assert_eq!(x.slots, y.slots);
        // Keychain items gone (the account half-removed): a clear error.
        let e = expand_with(&store, &src, Secrets::new(), |_: &str, _: &str| Ok(None)).err().unwrap();
        assert!(e.contains("reconectala"), "{e}");
        // Asked: the connection's typed passphrase, else one asked for the
        // account this run; nothing kept in the keychain is read.
        let asked_id = "6f1c2d4e-0000-4000-8000-000000000002";
        store.save_secret_account(&account(asked_id, MasterSource::Ask)).unwrap();
        let src: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"zoho","secret_name":"pg","account":"{asked_id}"}}"#)).unwrap();
        let x = expand_with(&store, &src, Secrets::new(), keychain(asked_id)).unwrap();
        assert!(x.slots.get("").is_none(), "not asked yet");
        let typed: Secrets = [(String::new(), "typed".to_string())].into();
        let x = expand_with(&store, &src, typed, keychain(asked_id)).unwrap();
        assert_eq!(x.slots.get("").map(String::as_str), Some("typed"));
        after_run(&x, Ok(()));
        let y = expand_with(&store, &src, Secrets::new(), keychain(asked_id)).unwrap();
        assert_eq!(y.slots.get("").map(String::as_str), Some("typed"), "another connection of the account: no second prompt");
        after_run(&y, Err(zoho::WRONG_PASSPHRASE));
        assert!(expand_with(&store, &src, Secrets::new(), keychain(asked_id)).unwrap().slots.get("").is_none(), "a wrong one is dropped");
        // Removed: its connections get the missing-account error.
        store.delete_secret_account(id).unwrap();
        let src: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"zoho","secret_name":"pg","account":"{id}"}}"#)).unwrap();
        assert_eq!(expand_with(&store, &src, Secrets::new(), keychain(id)).err().unwrap(), MISSING_ACCOUNT);
    }

    // ---- every other manager ------------------------------------------------------------

    fn acct(id: &str, config: AccountConfig) -> SecretAccount {
        SecretAccount { id: id.into(), label: format!("{} acct", config.kind()), identity: String::new(), config, created_at: String::new(), updated_at: String::new() }
    }

    /// A keychain of `id` with every slot `<slot>-value`.
    fn kept(id: &str) -> impl Fn(&str, &str) -> dbine_core::Result<Option<String>> {
        let id = id.to_string();
        move |a: &str, slot: &str| Ok((a == id).then(|| format!("{slot}-value")))
    }

    /// A definition of each kind through an account of its kind runs as
    /// the inline one with the account's sign-in, which validates; the
    /// keychain slots go where the inline form keeps them.
    #[test]
    fn every_kind_expands_through_its_account() {
        let store = StateStore::open_in_memory().unwrap();
        let id = "6f1c2d4e-0000-4000-8000-000000000010";
        let cases: Vec<(AccountConfig, &str, Vec<(&str, &str)>)> = vec![
            (
                AccountConfig::Bitwarden { server: "bitwarden.eu".into(), client_id: "user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04".into(), master: MasterSource::Keychain, master_command: String::new() },
                r#"{"kind":"bitwarden","server":"","item":"pg","fill_username":true,"secret_account":"ID"}"#,
                vec![("client_secret", "client_secret-value"), ("", "passphrase-value")],
            ),
            (AccountConfig::BitwardenSm { server: "bitwarden.com".into() }, r#"{"kind":"bitwarden_sm","server":"","secret":"1c9d6c0e-55aa-4b6f-8a43-0a1b2c3d4e5f","secret_account":"ID"}"#, vec![("", "token-value")]),
            (
                AccountConfig::Vault { address: "https://vault.example.com:8200".into(), namespace: "admin".into(), ca_file: String::new(), auth: VaultAuth::Token, approle_mount: String::new(), role_id: String::new() },
                r#"{"kind":"vault","path":"secret/app","key":"pw","secret_account":"ID"}"#,
                vec![("", "token-value")],
            ),
            (
                AccountConfig::Vault { address: "http://127.0.0.1:8200".into(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Approle, approle_mount: String::new(), role_id: "r-1".into() },
                r#"{"kind":"vault_db","role":"ro","secret_account":"ID"}"#,
                vec![("secret_id", "secret_id-value")],
            ),
            (
                AccountConfig::Delinea { url: "https://ss.example.com".into(), platform_url: String::new(), username: "svc".into(), domain: String::new(), master: MasterSource::Keychain, master_command: String::new() },
                r#"{"kind":"delinea","secret_name":"pg","secret_account":"ID"}"#,
                vec![("", "password-value")],
            ),
            (AccountConfig::Conjur { url: "https://conjur".into(), conjur_account: "myorg".into(), login: "host/x".into(), ca_file: String::new() }, r#"{"kind":"conjur","variable":"db/pw","secret_account":"ID"}"#, vec![("", "api_key-value")]),
            (
                AccountConfig::Ccp { url: "https://ccp".into(), app_id: "DBine".into(), client_cert: "~/c.p12".into(), client_key: String::new(), ca_file: String::new() },
                r#"{"kind":"ccp","safe":"DB","object":"pg","secret_account":"ID"}"#,
                vec![("", "passphrase-value")],
            ),
            (AccountConfig::OnePassword { account: "my.1password.com".into() }, r#"{"kind":"onepassword","reference":"op://P/DB/password","secret_account":"ID"}"#, vec![]),
            (AccountConfig::Aws { profile: "prod".into(), region: "eu-west-1".into() }, r#"{"kind":"aws","secret_id":"prod/db","secret_account":"ID"}"#, vec![]),
            (AccountConfig::Aws { profile: String::new(), region: "eu-west-1".into() }, r#"{"kind":"ssm","parameter":"/prod/db","secret_account":"ID"}"#, vec![]),
            (AccountConfig::Azure { tenant: "contoso.onmicrosoft.com".into(), vault_url: "https://kv1.vault.azure.net".into() }, r#"{"kind":"azure","name":"db-pw","secret_account":"ID"}"#, vec![]),
            (AccountConfig::Gcp { project: "my-proj".into() }, r#"{"kind":"gcp","secret":"db-pw","secret_account":"ID"}"#, vec![]),
            (
                AccountConfig::KeePass { file: "~/db.kdbx".into(), key_file: "~/db.keyx".into(), master: MasterSource::Keychain, master_command: String::new() },
                r#"{"kind":"keepass","entry":"Databases/pg","secret_account":"ID"}"#,
                vec![("", "passphrase-value")],
            ),
        ];
        for (config, json, want) in cases {
            store.save_secret_account(&acct(id, config.clone())).unwrap();
            let src: SecretSource = serde_json::from_str(&json.replace("ID", id)).unwrap();
            // The field's own typed slots are dropped (only an asked master password counts).
            let field: Secrets = [("client_secret".to_string(), "from-the-field".to_string())].into();
            let x = expand_with(&store, &src, field, kept(id)).unwrap_or_else(|e| panic!("{json}: {e}"));
            assert_eq!(x.src.kind(), src.kind());
            assert_eq!(x.src.account(), None, "inline: {json}");
            x.src.validate().unwrap_or_else(|e| panic!("{json}: {e}"));
            let mut got: Vec<(&str, &str)> = x.slots.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            got.sort();
            let mut want = want;
            want.sort();
            assert_eq!(got, want, "{json}");
            // The account's sign-in is what runs.
            let inline = serde_json::to_string(&x.src).unwrap();
            let config_json = serde_json::to_value(&config).unwrap();
            for (k, v) in config_json.as_object().unwrap() {
                let k = if k == "conjur_account" { "conjur_account" } else { k.as_str() };
                if k != "kind" && v.as_str().is_some_and(|v| !v.is_empty()) {
                    assert!(inline.contains(&format!("\"{k}\":{v}")), "{json}: {k} in {inline}");
                }
            }
            // Another kind's account: refused.
            let wrong: SecretSource = serde_json::from_str(&json.replace("ID", id)).unwrap();
            let other = if config.kind() == "gcp" { AccountConfig::OnePassword { account: "my".into() } } else { AccountConfig::Gcp { project: "p".into() } };
            store.save_secret_account(&acct(id, other)).unwrap();
            assert!(expand_with(&store, &wrong, Secrets::new(), kept(id)).err().unwrap().contains("otro gestor"), "{json}");
        }
        // A missing account names its manager.
        let src: SecretSource = serde_json::from_str(r#"{"kind":"vault_db","role":"ro","secret_account":"6f1c2d4e-0000-4000-8000-00000000dead"}"#).unwrap();
        assert_eq!(expand(&store, &src, Secrets::new()).err().unwrap(), missing_account("vault"));
        assert!(missing_account("vault").contains("HashiCorp Vault"));
        // Required keychain items gone: reconnect.
        store.save_secret_account(&acct(id, AccountConfig::Conjur { url: "https://conjur".into(), conjur_account: "a".into(), login: "l".into(), ca_file: String::new() })).unwrap();
        let src: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"conjur","variable":"v","secret_account":"{id}"}}"#)).unwrap();
        assert!(expand_with(&store, &src, Secrets::new(), |_: &str, _: &str| Ok(None)).err().unwrap().contains("reconectala"));
        // A Vault token not kept: the environment's (VAULT_TOKEN, ~/.vault-token).
        store.save_secret_account(&acct(id, AccountConfig::Vault { address: "https://v".into(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Token, approle_mount: String::new(), role_id: String::new() })).unwrap();
        let src: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"vault","path":"a/b","key":"k","secret_account":"{id}"}}"#)).unwrap();
        assert!(expand_with(&store, &src, Secrets::new(), |_: &str, _: &str| Ok(None)).unwrap().slots.is_empty());
    }

    /// An asked master password (Bitwarden, Delinea, KeePass): the
    /// connection's typed one, else one asked for the account this run; a
    /// wrong one is dropped. A command's: the inline form runs it.
    #[test]
    fn asked_and_command_masters_through_an_account() {
        let store = StateStore::open_in_memory().unwrap();
        let id = "6f1c2d4e-0000-4000-8000-000000000020";
        store.save_secret_account(&acct(id, AccountConfig::Bitwarden { server: String::new(), client_id: "user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04".into(), master: MasterSource::Ask, master_command: String::new() })).unwrap();
        let src: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"bitwarden","server":"","item":"pg","secret_account":"{id}"}}"#)).unwrap();
        let x = expand_with(&store, &src, Secrets::new(), kept(id)).unwrap();
        assert!(!x.slots.contains_key(""), "not asked yet");
        assert_eq!(x.src.master(), Some(MasterSource::Ask), "the inline form asks (the unlock dialog)");
        let x = expand_with(&store, &src, [(String::new(), "typed".to_string())].into(), kept(id)).unwrap();
        after_run(&x, Ok(()));
        let y = expand_with(&store, &src, Secrets::new(), kept(id)).unwrap();
        assert_eq!(y.slots.get("").map(String::as_str), Some("typed"), "another connection of the account");
        after_run(&y, Err(bitwarden::WRONG_PASSWORD));
        assert!(expand_with(&store, &src, Secrets::new(), kept(id)).unwrap().slots.get("").is_none(), "a wrong one is dropped");
        store
            .save_secret_account(&acct(id, AccountConfig::KeePass { file: "~/db.kdbx".into(), key_file: String::new(), master: MasterSource::Command, master_command: "pass kp".into() }))
            .unwrap();
        let src: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"keepass","entry":"pg","secret_account":"{id}"}}"#)).unwrap();
        let x = expand_with(&store, &src, [(String::new(), "typed".to_string())].into(), kept(id)).unwrap();
        assert!(matches!(&x.src, SecretSource::KeePass { master: MasterSource::Command, master_command, .. } if master_command == "pass kp"));
        assert!(x.slots.is_empty(), "nothing typed or kept is used with a command");
        forget_asked(id);
    }

    /// Reconnecting reuses the secrets kept for the account only while it
    /// goes to the same place; typed ones win; only known slots count.
    #[test]
    fn reconnecting_reuses_kept_secrets_for_the_same_endpoint_only() {
        let id = "6f1c2d4e-0000-4000-8000-000000000030";
        let vault = |address: &str| AccountConfig::Vault { address: address.into(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Token, approle_mount: String::new(), role_id: String::new() };
        let old = acct(id, vault("https://vault.example.com"));
        let none = Typed::default();
        let s = gather(&vault("https://vault.example.com/"), &none, Some(&old), kept(id)).unwrap();
        assert_eq!(s.get("token").map(String::as_str), Some("token-value"), "same Vault: the kept token");
        let s = gather(&vault("https://other.example.com"), &none, Some(&old), kept(id)).unwrap();
        assert!(s.is_empty(), "another address: typed again, the old token never goes there");
        let typed = Typed([("token".to_string(), "new".to_string()), ("evil_slot".to_string(), "x".to_string()), ("api_key".to_string(), "y".to_string())].into());
        let s = gather(&vault("https://vault.example.com"), &typed, Some(&old), kept(id)).unwrap();
        assert_eq!(s.into_iter().collect::<Vec<_>>(), [("token".to_string(), "new".to_string())], "only the account's own slots");
        // An asked master password is checked but isn't a slot the account keeps.
        let bw = AccountConfig::Bitwarden { server: String::new(), client_id: "user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04".into(), master: MasterSource::Ask, master_command: String::new() };
        let typed = Typed([("client_secret".to_string(), "cs".to_string()), ("passphrase".to_string(), "pw".to_string())].into());
        let checked = gather(&bw, &typed, None, kept(id)).unwrap();
        assert_eq!(checked.len(), 2);
        let (account, keep) = finish_generic(&bw, "", "ana@example.com", None, &checked).unwrap();
        assert_eq!(keep.iter().map(|(s, _)| *s).collect::<Vec<_>>(), ["client_secret"], "the asked password is never kept");
        assert_eq!(account.label, "Bitwarden · ana@example.com");
        let row = serde_json::to_string(&account).unwrap();
        assert!(!row.contains("\"cs\"") && !row.contains("\"pw\""), "{row}");
        // Kept in the keychain: kept.
        let bw_kept = AccountConfig::Bitwarden { server: String::new(), client_id: "user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04".into(), master: MasterSource::Keychain, master_command: String::new() };
        let (_, keep) = finish_generic(&bw_kept, "Mine", "", None, &checked).unwrap();
        assert_eq!(keep.iter().map(|(s, _)| *s).collect::<Vec<_>>(), ["client_secret", "passphrase"]);
        // Reconnecting keeps the ID and the creation time.
        let existing = SecretAccount { created_at: "2026-01-01T00:00:00Z".into(), ..acct(id, bw.clone()) };
        let (again, _) = finish_generic(&bw, "x", "ana@example.com", Some(&existing), &checked).unwrap();
        assert_eq!((again.id.as_str(), again.created_at.as_str()), (id, "2026-01-01T00:00:00Z"));
    }

    #[test]
    fn default_labels() {
        let l = |c: AccountConfig, who: &str| default_label(&c, who);
        assert_eq!(l(AccountConfig::Aws { profile: String::new(), region: "eu-west-1".into() }, "arn:aws:iam::1:user/a"), "AWS · default · eu-west-1");
        assert_eq!(l(AccountConfig::Azure { tenant: String::new(), vault_url: "https://kv1.vault.azure.net".into() }, "ana"), "Azure Key Vault · kv1");
        assert_eq!(l(AccountConfig::Vault { address: "https://vault.example.com:8200/".into(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Token, approle_mount: String::new(), role_id: String::new() }, "token"), "HashiCorp Vault · vault.example.com:8200");
        assert_eq!(l(AccountConfig::BitwardenSm { server: "bitwarden.eu".into() }, "org"), "Bitwarden Secrets Manager · bitwarden.eu");
        assert_eq!(l(AccountConfig::OnePassword { account: "my".into() }, ""), "1Password");
    }

    /// The ambient wizards against fake `op`, `az` and `gcloud` on PATH:
    /// what's detected, and the check that names who signed in. It changes
    /// PATH, so it runs alone:
    /// `DBINE_TEST_FAKE_CLIS=1 cargo test -p dbine fake_clis -- --ignored --test-threads=1`.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn fake_clis_for_the_wizards() {
        use base64::Engine;
        use std::os::unix::fs::PermissionsExt;
        if std::env::var("DBINE_TEST_FAKE_CLIS").is_err() {
            return;
        }
        let dir = super::super::tests::temp_dir("fake-clis");
        let jwt = format!("x.{}.y", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"upn":"ana@contoso.com","tid":"11111111-2222-4333-8444-555555555555"}"#));
        let write = |name: &str, body: String| {
            let f = dir.join(name);
            std::fs::write(&f, body).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        write(
            "op",
            "#!/bin/sh\ncase \"$*\" in\n  'account list --format=json') echo '[{\"url\":\"my.1password.com\",\"email\":\"ana@example.com\",\"user_uuid\":\"U1\",\"account_uuid\":\"A1\",\"secret\":\"never\"}]' ;;\n  'whoami --format=json --account=my.1password.com') echo '{\"url\":\"my.1password.com\",\"email\":\"ana@example.com\",\"user_uuid\":\"U1\"}' ;;\n  *) echo '[ERROR] 2026/10/18 10:00:00 account is not signed in' >&2; exit 1 ;;\nesac\n".into(),
        );
        write(
            "az",
            format!("#!/bin/sh\ncase \"$*\" in\n  'account show --output json') echo '{{\"tenantId\":\"11111111-2222-4333-8444-555555555555\",\"name\":\"Pay-As-You-Go\",\"user\":{{\"name\":\"ana@contoso.com\",\"type\":\"user\"}}}}' ;;\n  'account get-access-token --resource https://vault.azure.net --output json'*) echo '{{\"accessToken\":\"{jwt}\",\"expires_on\":4102444800}}' ;;\n  *) exit 3 ;;\nesac\n"),
        );
        write(
            "gcloud",
            "#!/bin/sh\ncase \"$*\" in\n  'config list --format json') echo '{\"core\":{\"account\":\"ana@example.com\",\"project\":\"my-proj\"}}' ;;\n  'auth print-access-token --quiet') echo ya29.fake ;;\n  'config get-value account --quiet') echo ana@example.com ;;\n  *) exit 3 ;;\nesac\n".into(),
        );
        let old = std::env::var_os("PATH").unwrap_or_default();
        std::env::set_var("PATH", &dir);
        let detect = |k: &str| detect_secret_account(DetectAccountArgs { kind: k.into() });
        let op = detect("onepassword").await.unwrap();
        assert_eq!(op["installed"], true);
        assert_eq!(op["accounts"][0]["url"], "my.1password.com");
        assert!(op["accounts"][0].get("secret").is_none(), "only the known fields");
        let az = detect("azure").await.unwrap();
        assert_eq!((az["tenant"].as_str(), az["user"].as_str()), (Some("11111111-2222-4333-8444-555555555555"), Some("ana@contoso.com")));
        let g = detect("gcp").await.unwrap();
        assert_eq!((g["account"].as_str(), g["project"].as_str()), (Some("ana@example.com"), Some("my-proj")));
        let none = Secrets::new();
        assert_eq!(verify(&AccountConfig::OnePassword { account: "my.1password.com".into() }, &none).await.unwrap().0, "ana@example.com");
        assert!(verify(&AccountConfig::OnePassword { account: "other.1password.com".into() }, &none).await.unwrap_err().contains("op signin"));
        let (who, detail) = verify(&AccountConfig::Azure { tenant: "11111111-2222-4333-8444-555555555555".into(), vault_url: "https://kv1.vault.azure.net".into() }, &none).await.unwrap();
        assert_eq!((who.as_str(), detail.as_str()), ("ana@contoso.com", "inquilino 11111111-2222-4333-8444-555555555555"));
        let e = verify(&AccountConfig::Azure { tenant: "99999999-2222-4333-8444-555555555555".into(), vault_url: "https://kv1.vault.azure.net".into() }, &none).await.unwrap_err();
        assert!(e.contains("es del inquilino"), "another tenant's token: {e}");
        assert_eq!(verify(&AccountConfig::Gcp { project: "my-proj".into() }, &none).await.unwrap().0, "ana@example.com");
        std::env::set_var("PATH", old);
    }

    /// Against the test containers: each wizard's check, then connections
    /// through the accounts, as `fill` runs them (`DBINE_TEST_PG_SECRETS`:
    /// and a real session on `dbine-test-postgres`). Every part runs when
    /// its variable is set; it changes the environment, so it runs alone:
    /// `DBINE_KEYCHAIN_SERVICE=io.addlayer.dbine.accounts-test DBINE_TEST_VAULT=http://127.0.0.1:28200
    /// DBINE_TEST_VAULT_APPROLE=<role_id>:<secret_id> DBINE_TEST_BITWARDEN=live.json
    /// DBINE_TEST_LOCALSTACK=http://127.0.0.1:24566 cargo test -p dbine live_accounts -- --ignored --test-threads=1`.
    #[tokio::test]
    #[ignore]
    async fn live_accounts() {
        use dbine_core::secret_sources::SecretSources;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = super::super::tests::temp_dir("accounts-live");
        let store = StateStore::open(&dir.join("s.sqlite")).unwrap();
        let pg = || dbine_driver::ConnectionConfig { driver: "postgres".into(), host: "127.0.0.1".into(), port: 25010, database: "postgres".into(), username: Some("someone-else".into()), ..Default::default() };
        let connect = |cfg: dbine_driver::ConnectionConfig, what: &'static str| async move {
            if std::env::var("DBINE_TEST_PG_SECRETS").is_ok() {
                let mut sess = dbine_drivers::open_session(&cfg, None).await.unwrap_or_else(|e| panic!("{what}: connect: {e}"));
                eprintln!("{what}: connected as {}, {}", cfg.username.as_deref().unwrap_or(""), sess.server_version().await.unwrap());
            }
        };
        let form = super::super::Origin::Form { id: None, name: "" };
        if let Ok(addr) = std::env::var("DBINE_TEST_VAULT") {
            // The wizard's check, with a token and with AppRole.
            let token = AccountConfig::Vault { address: addr.clone(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Token, approle_mount: String::new(), role_id: String::new() };
            let (name, policies) = verify(&token, &[("token".to_string(), "dbine-root".to_string())].into()).await.unwrap();
            eprintln!("vault token: {name} {policies}");
            assert!(name == "token" && policies.contains("root"), "{name} {policies}");
            assert!(verify(&token, &[("token".to_string(), "nope".to_string())].into()).await.unwrap_err().contains("rechazó el token"));
            if let Ok(ar) = std::env::var("DBINE_TEST_VAULT_APPROLE") {
                let (role_id, secret_id) = ar.trim().split_once(':').unwrap();
                let approle = AccountConfig::Vault { address: addr.clone(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Approle, approle_mount: String::new(), role_id: role_id.into() };
                let (name, policies) = verify(&approle, &[("secret_id".to_string(), secret_id.to_string())].into()).await.unwrap();
                eprintln!("vault approle: {name} {policies}");
                assert!(name.starts_with("approle") && policies.contains("dbine-db"), "{name} {policies}");
                // A KV secret through the AppRole account (its secret ID as the keychain would give it).
                let id = "6f1c2d4e-0000-4000-8000-0000000000a1";
                store.save_secret_account(&acct(id, approle)).unwrap();
                let kv: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"vault","path":"secret/dbine/pg","key":"password","secret_account":"{id}"}}"#)).unwrap();
                let x = expand_with(&store, &kv, Secrets::new(), |_: &str, slot: &str| Ok((slot == "secret_id").then(|| secret_id.to_string()))).unwrap();
                assert_eq!(super::super::fetch(&x.src, &x.slots).await.unwrap().value, "pw");
            }
            // A connection through the token account, as `fill` runs it: KV,
            // then the database engine's own credentials for the session.
            // The account keeps no token: VAULT_TOKEN is used.
            std::env::set_var("VAULT_TOKEN", "dbine-root");
            let id = "6f1c2d4e-0000-4000-8000-0000000000a2";
            store.save_secret_account(&acct(id, token)).unwrap();
            let mut cfg = pg();
            let sources: SecretSources = [("password".to_string(), serde_json::from_str(&format!(r#"{{"kind":"vault","path":"secret/dbine/pg","key":"password","secret_account":"{id}"}}"#)).unwrap())].into();
            super::super::fill(&store, &mut cfg, &sources, &Secrets::new(), form).await.unwrap();
            assert_eq!(cfg.password.as_deref(), Some("pw"));
            let mut cfg = pg();
            let sources: SecretSources = [("password".to_string(), serde_json::from_str(&format!(r#"{{"kind":"vault_db","role":"ro","secret_account":"{id}"}}"#)).unwrap())].into();
            super::super::fill(&store, &mut cfg, &sources, &Secrets::new(), form).await.unwrap();
            let user = cfg.username.clone().unwrap();
            assert!(user.starts_with("v-") && cfg.password.as_deref().is_some_and(|p| !p.is_empty()), "{user}");
            connect(cfg, "vault account (database credentials)").await;
            std::env::remove_var("VAULT_TOKEN");
        }
        if let Ok(path) = std::env::var("DBINE_TEST_BITWARDEN") {
            let live: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            let s = |v: &Value, k: &str| v[k].as_str().unwrap().to_string();
            let u = &live["users"][0];
            let config = AccountConfig::Bitwarden { server: s(&live, "server"), client_id: s(u, "client_id"), master: MasterSource::Keychain, master_command: String::new() };
            let typed: Secrets = [("client_secret".to_string(), s(u, "client_secret")), ("passphrase".to_string(), s(u, "password"))].into();
            let (email, _) = verify(&config, &typed).await.unwrap();
            assert_eq!(email, s(u, "email"));
            let wrong: Secrets = [("client_secret".to_string(), s(u, "client_secret")), ("passphrase".to_string(), "nope".to_string())].into();
            assert_eq!(verify(&config, &wrong).await.unwrap_err(), bitwarden::WRONG_PASSWORD);
            let id = "6f1c2d4e-0000-4000-8000-0000000000b1";
            store.save_secret_account(&acct(id, config)).unwrap();
            let src: SecretSource = serde_json::from_str(&format!(r#"{{"kind":"bitwarden","server":"","item":"dbine-test-postgres","fill_username":true,"secret_account":"{id}"}}"#)).unwrap();
            let x = expand_with(&store, &src, Secrets::new(), |_: &str, slot: &str| Ok(Some(if slot == "client_secret" { s(u, "client_secret") } else { s(u, "password") }))).unwrap();
            let r = super::super::fetch(&x.src, &x.slots).await.unwrap();
            assert_eq!((r.value.as_str(), r.username.as_deref()), ("pw", Some("postgres")));
            let mut cfg = pg();
            cfg.password = Some(r.value.clone());
            cfg.username = r.username.clone();
            connect(cfg, "bitwarden account").await;
        }
        if let Ok(url) = std::env::var("DBINE_TEST_LOCALSTACK") {
            std::env::set_var("AWS_ENDPOINT_URL_STS", &url);
            std::env::set_var("AWS_ENDPOINT_URL_SSM", &url);
            std::env::set_var("AWS_ACCESS_KEY_ID", "test");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
            let config = AccountConfig::Aws { profile: String::new(), region: "us-east-1".into() };
            let (arn, _) = verify(&config, &Secrets::new()).await.unwrap();
            eprintln!("aws: {arn}");
            assert!(arn.starts_with("arn:aws:"), "{arn}");
            let id = "6f1c2d4e-0000-4000-8000-0000000000c1";
            store.save_secret_account(&acct(id, config)).unwrap();
            let mut cfg = pg();
            let sources: SecretSources = [("password".to_string(), serde_json::from_str(&format!(r#"{{"kind":"ssm","parameter":"/dbine/pg","json_key":"password","fill_username":true,"secret_account":"{id}"}}"#)).unwrap())].into();
            super::super::fill(&store, &mut cfg, &sources, &Secrets::new(), form).await.unwrap();
            assert_eq!((cfg.password.as_deref(), cfg.username.as_deref()), (Some("pw"), Some("postgres")));
            connect(cfg, "aws account (parameter store)").await;
            for k in ["AWS_ENDPOINT_URL_STS", "AWS_ENDPOINT_URL_SSM", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] {
                std::env::remove_var(k);
            }
        }
    }
}

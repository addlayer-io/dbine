//! Secret-manager accounts (dbine_core::secret_accounts): connecting one
//! (the "Conectar Zoho Vault" wizard), renaming, reconnecting and removing
//! them, and resolving a definition that names one.
//!
//! - Connecting keeps nothing until the end: the code is exchanged and the
//!   result (the refresh token, the passphrase's check data) waits in memory
//!   under a pending ID; "finish" verifies the passphrase and only then
//!   writes the keychain and the account's row. Cancelling wipes it (and
//!   revokes the refresh token at Zoho, best effort).
//! - The code and the client secret only go to Zoho's accounts servers
//!   (zoho::accounts_servers), over HTTPS, without following redirects.
//! - A definition with `account` is expanded here into the inline form, with
//!   the account's data centre, client, passphrase choice and keychain
//!   secrets, right before it runs; the confirmation of imported
//!   definitions is checked before that, on the definition as stored.
//! - Access tokens are cached by the account's credentials (zoho.rs), so
//!   every connection using an account shares one: Zoho allows 10 refreshes
//!   per 10 minutes.
//! - These commands are the connection form's and Settings' only: MCP, the
//!   assistant and the phone have no way to them.

use super::zoho;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::secret_accounts::{self as accts, AccountConfig, SecretAccount};
use dbine_core::secret_sources::{self as defs, wipe, MasterSource, SecretSource};
use dbine_core::secrets::Secrets;
use dbine_core::StateStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tauri::State;

/// The definition names an account this computer doesn't have (an import,
/// a restored backup, an account removed). The form shows the account picker.
pub const MISSING_ACCOUNT: &str = "la cuenta de Zoho Vault de esta conexión no está en esta computadora: elegí o conectá una cuenta de Zoho Vault en la conexión";

/// The passphrase typed in the wizard didn't open the account.
pub const PASSPHRASE_MISMATCH: &str = "la frase no coincide con la frase de contraseña maestra de esta cuenta de Zoho Vault";

// ---- resolving --------------------------------------------------------------------

/// Passphrases asked for an account ("ask when connecting") this run: one
/// typed for any of its connections serves the others. Never on disk.
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
    /// The account it came from, with its passphrase choice.
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
/// passphrase asked for the connection, in `""`).
pub fn expand(store: &StateStore, src: &SecretSource, slots: Secrets) -> Result<Expanded, String> {
    expand_with(store, src, slots, accts::get_secret)
}

/// The same, reading the account's secrets with `get` (the keychain, or a
/// map in tests).
pub fn expand_with(store: &StateStore, src: &SecretSource, mut slots: Secrets, get: impl Fn(&str, &str) -> dbine_core::Result<Option<String>>) -> Result<Expanded, String> {
    let Some(id) = src.zoho_account() else { return Ok(Expanded { src: src.clone(), slots, account: None }) };
    let SecretSource::Zoho { secret_name, attribute, fill_username, user_field, .. } = src else { unreachable!("zoho_account") };
    let account = store.get_secret_account(id).map_err(|e| e.to_string())?.ok_or_else(|| MISSING_ACCOUNT.to_string())?;
    let AccountConfig::Zoho { data_center, client_id, master, master_command } = &account.config;
    let mut out = Secrets::new();
    for slot in ["client_secret", "refresh_token"] {
        let v = get(id, slot).map_err(|e| format!("no se pudo leer la cuenta de Zoho Vault del llavero: {e}"))?;
        let v = v.filter(|v| !v.is_empty()).ok_or("a la cuenta de Zoho Vault le faltan datos en el llavero: reconectala en Configuración › Cuentas de gestores de secretos")?;
        out.insert(slot.to_string(), v);
    }
    let typed = slots.remove("").filter(|t| !t.is_empty());
    let passphrase = match master {
        MasterSource::Keychain => get(id, "passphrase").map_err(|e| format!("no se pudo leer la cuenta de Zoho Vault del llavero: {e}"))?,
        MasterSource::Ask => typed.or_else(|| ASKED.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()),
        MasterSource::Command | MasterSource::None => None,
    };
    if let Some(p) = passphrase {
        out.insert(String::new(), p);
    }
    for v in slots.values_mut() {
        wipe(v);
    }
    let inline = SecretSource::Zoho {
        data_center: data_center.clone(),
        secret_name: secret_name.clone(),
        attribute: attribute.clone(),
        client_id: client_id.clone(),
        master: *master,
        master_command: master_command.clone(),
        fill_username: *fill_username,
        user_field: user_field.clone(),
        account: String::new(),
    };
    Ok(Expanded { src: inline, slots: out, account: Some((id.to_string(), *master)) })
}

/// After a run through an account: an asked passphrase that worked is kept
/// for its other connections; one that didn't is dropped.
pub fn after_run(e: &Expanded, ok: Result<(), &str>) {
    let Some((id, MasterSource::Ask)) = &e.account else { return };
    match ok {
        Ok(()) => {
            if let Some(p) = e.slots.get("") {
                ASKED.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), p.clone());
            }
        }
        Err(m) if m == zoho::WRONG_PASSPHRASE => forget_asked(id),
        Err(_) => {}
    }
}

/// The access token kept for an account (removed, reconnected) is dropped.
fn forget_account_token(a: &SecretAccount) {
    let AccountConfig::Zoho { data_center, client_id, .. } = &a.config;
    let creds = zoho::Creds {
        client_id: client_id.clone(),
        client_secret: accts::get_secret(&a.id, "client_secret").ok().flatten().unwrap_or_default(),
        refresh_token: accts::get_secret(&a.id, "refresh_token").ok().flatten().unwrap_or_default(),
        passphrase: String::new(),
    };
    zoho::forget_token_of(data_center, &creds);
}

// ---- connecting (the wizard) --------------------------------------------------------

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
}

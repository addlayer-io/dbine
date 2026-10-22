//! Zoho Vault: one field of a secret, over Zoho Vault's REST API (v1) with
//! an OAuth self client, decrypted here. Zoho Vault encrypts every value in
//! the client with the user's master passphrase, so the API only returns
//! ciphertext:
//!
//! 1. An access token from the refresh token (`accounts.<dc>/oauth/v2/token`).
//! 2. `GET_LOGIN`: the salt, the iteration count and a check value.
//! 3. The master key: PBKDF2-HMAC-SHA256(passphrase, salt, iterations) as hex
//!    (`PBKDF2_AES` accounts), or SHA-256(passphrase) as hex (older
//!    `SHA256` accounts).
//! 4. The secret, by ID, or by name looked for in the secrets list (read
//!    with its paging arguments only: Zoho refuses `passwordName` with
//!    EXTRA_PARAM_FOUND). A personal secret is encrypted with the
//!    master key; a shared one (`isshared: YES`) with the organization key,
//!    which is RSA-encrypted for the user with a private key that is itself
//!    encrypted with the master key (`/user/orgkey`): jsbn's hex key and
//!    PKCS#1 v1.5 (`RSA_1024`, the default) or PKCS#8 and OAEP (`RSA_4096`).
//! 5. Each value: base64 of an 8-byte nonce and AES-256 in counter mode, the
//!    cipher key being the first 16 bytes of the key text encrypted with
//!    AES-256 under itself, twice (Zoho's web client uses Chris Veness'
//!    AES-CTR with 256 bits).
//!
//! Tokens only go to the two hosts of the chosen data centre
//! (dbine_core::secret_sources::ZOHO_DATA_CENTERS), over HTTPS, without
//! following redirects. Nothing here logs a value; errors carry Zoho's codes,
//! never a token or a secret.

use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use aes::Aes256;
use base64::Engine;
use dbine_core::secret_sources::{wipe, wipe_bytes, zoho_hosts};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// The master passphrase didn't decrypt the account's check value. `fill`
/// checks for this exact text to ask an asked-for passphrase again.
pub const WRONG_PASSPHRASE: &str = "la frase de contraseña maestra de Zoho Vault no es la correcta";

/// The self client's secrets and the master passphrase; wiped on drop.
pub struct Creds {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub passphrase: String,
}

impl Drop for Creds {
    fn drop(&mut self) {
        wipe(&mut self.client_secret);
        wipe(&mut self.refresh_token);
        wipe(&mut self.passphrase);
    }
}

pub struct Request<'a> {
    pub data_center: &'a str,
    /// A name, or an ID (digits).
    pub secret: &'a str,
    /// Empty: `password`.
    pub attribute: &'a str,
    pub username: bool,
    /// The secret's field the user comes from; empty: `username`.
    pub user_field: &'a str,
}

/// The base URLs to talk to: Zoho's for a data centre; a mock in tests.
pub struct Endpoints {
    pub accounts: String,
    pub vault: String,
}

impl Endpoints {
    /// The data centre's hosts, over HTTPS. Nothing else is ever used
    /// outside the tests.
    pub fn of(data_center: &str) -> Option<Self> {
        let (accounts, vault) = zoho_hosts(data_center)?;
        Some(Self {
            accounts: format!("https://{accounts}"),
            vault: format!("https://{vault}"),
        })
    }
}

/// `(value, username)`.
pub async fn read(req: &Request<'_>, creds: &Creds) -> Result<(String, Option<String>), String> {
    let ep = Endpoints::of(req.data_center).ok_or_else(|| {
        format!(
            "el centro de datos de Zoho «{}» no es válido",
            req.data_center.trim()
        )
    })?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    read_with(&client, &ep, req, creds).await
}

// ---- access tokens -----------------------------------------------------------

/// Access tokens by (data centre, client, refresh token) digest: Zoho limits
/// how often a refresh token may be used, and a token lasts an hour.
static TOKENS: LazyLock<Mutex<HashMap<String, (Instant, String)>>> = LazyLock::new(Mutex::default);

fn token_cache_key(ep: &Endpoints, creds: &Creds) -> String {
    let mut h = Sha256::new();
    for part in [
        ep.accounts.as_str(),
        creds.client_id.as_str(),
        creds.refresh_token.as_str(),
    ] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    super::cloud_auth::hex(&h.finalize())
}

fn forget_token(key: &str) {
    if let Some((_, mut t)) = TOKENS.lock().unwrap_or_else(|e| e.into_inner()).remove(key) {
        wipe(&mut t);
    }
}

async fn access_token(
    client: &reqwest::Client,
    ep: &Endpoints,
    creds: &Creds,
) -> Result<String, String> {
    let key = token_cache_key(ep, creds);
    {
        let mut c = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
        c.retain(|_, (until, t)| {
            let keep = *until > Instant::now();
            if !keep {
                wipe(t);
            }
            keep
        });
        if let Some((_, t)) = c.get(&key) {
            return Ok(t.clone());
        }
    }
    if creds.client_secret.trim().is_empty() || creds.refresh_token.trim().is_empty() {
        return Err("faltan el secreto del cliente y el token de actualización de Zoho: escribilos en el formulario".into());
    }
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", creds.client_id.trim()),
        ("client_secret", creds.client_secret.trim()),
        ("refresh_token", creds.refresh_token.trim()),
    ];
    let resp = client
        .post(format!("{}/oauth/v2/token", ep.accounts))
        .form(&form)
        .send()
        .await
        .map_err(|e| {
            format!(
                "no se pudo conectar con Zoho ({}): {}",
                ep.accounts,
                e.without_url()
            )
        })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(t) = json
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        let secs = json
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(3600)
            .clamp(120, 3600);
        TOKENS.lock().unwrap_or_else(|e| e.into_inner()).insert(
            key,
            (
                Instant::now() + Duration::from_secs(secs - 60),
                t.to_string(),
            ),
        );
        return Ok(t.to_string());
    }
    let code = json.get("error").and_then(Value::as_str).unwrap_or("");
    let desc = json
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("");
    Err(oauth_error(status, code, desc))
}

pub fn oauth_error(status: u16, code: &str, desc: &str) -> String {
    match code {
        "invalid_client" | "invalid_client_secret" => {
            "Zoho rechazó el ID o el secreto del cliente (¿son de este centro de datos?)".into()
        }
        "invalid_code" | "invalid_grant" => {
            "el token de actualización de Zoho no es válido, venció o se revocó: generá uno nuevo"
                .into()
        }
        "Access Denied" | "access_denied" => format!(
            "Zoho no permite pedir tokens tan seguido; probá en unos minutos ({})",
            super::exec::first_lines(desc, 200)
        ),
        "" if status == 200 => "Zoho no devolvió un token de acceso".into(),
        "" => format!("Zoho respondió {status} al pedir el token de acceso"),
        _ => format!(
            "Zoho rechazó el pedido del token de acceso: {}",
            super::exec::first_lines(code, 100)
        ),
    }
}

/// Forget the access token of these credentials (an account removed or
/// reconnected): the next use asks Zoho for a new one.
pub fn forget_token_of(data_center: &str, creds: &Creds) {
    if let Some(ep) = Endpoints::of(data_center) {
        forget_token(&token_cache_key(&ep, creds));
    }
}

/// Keep an access token just issued (connecting an account), so the first
/// use doesn't spend one of Zoho's refreshes.
fn seed_token(ep: &Endpoints, creds: &Creds, token: &str, expires_in: u64) {
    let secs = expires_in.clamp(120, 3600);
    TOKENS.lock().unwrap_or_else(|e| e.into_inner()).insert(
        token_cache_key(ep, creds),
        (
            Instant::now() + Duration::from_secs(secs - 60),
            token.to_string(),
        ),
    );
}

// ---- connecting an account (the wizard) -----------------------------------------

/// The scopes the code has to be generated with: the secrets (names, data,
/// the organization key) and `GET_LOGIN` (the passphrase check, the user).
pub const SCOPES: &str = "ZohoVault.secrets.READ,ZohoVault.user.READ";

/// Accounts servers that Zoho lists (`/oauth/serverinfo`) but where DBine
/// doesn't know Zoho Vault's host: a Self Client found there gets a clear
/// error instead of "not recognized anywhere".
pub const ACCOUNTS_WITHOUT_VAULT: &[(&str, &str)] = &[("inec", "accounts.zohohq.in")];

/// The code expired or was already used: the user generates a new one.
pub const CODE_EXPIRED: &str =
    "el código de Zoho venció o ya se usó: generá un código nuevo en la pestaña «Generate Code»";
pub const UNKNOWN_CLIENT: &str = "Zoho no reconoce ese ID de cliente en ningún centro de datos: copiá el «Client ID» de la pestaña «Client Secret» de tu Self Client";
pub const WRONG_CLIENT_SECRET: &str =
    "Zoho rechazó el secreto del cliente: copiá de nuevo el «Client Secret» de tu Self Client";

/// What exchanging the code gave.
pub struct Exchanged {
    /// The data centre the Self Client answered from.
    pub data_center: String,
    pub refresh_token: String,
    pub access_token: String,
    pub expires_in: u64,
}

impl Drop for Exchanged {
    fn drop(&mut self) {
        wipe(&mut self.refresh_token);
        wipe(&mut self.access_token);
    }
}

/// Every accounts server the code may be tried at, in order: Zoho's data
/// centres (with Vault), then the ones without a known Vault host. Only
/// these, over HTTPS.
pub fn accounts_servers() -> Vec<(String, String)> {
    dbine_core::secret_sources::ZOHO_DATA_CENTERS
        .iter()
        .map(|(id, accounts, _)| (id.to_string(), format!("https://{accounts}")))
        .chain(
            ACCOUNTS_WITHOUT_VAULT
                .iter()
                .map(|(id, accounts)| (id.to_string(), format!("https://{accounts}"))),
        )
        .collect()
}

/// A client for Zoho: HTTPS only, no redirects, short timeouts.
pub fn https_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
}

/// Exchange a code generated in the Self Client for a refresh token, at
/// Zoho's own data centres. A Self Client exists in its owner's data
/// centre only, so the others answer `invalid_client`: the first that
/// doesn't is the one.
pub async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
) -> Result<Exchanged, String> {
    exchange_code_with(
        &https_client()?,
        &accounts_servers(),
        client_id,
        client_secret,
        code,
    )
    .await
}

/// The same, against `servers` (`(data centre, accounts base URL)`).
pub async fn exchange_code_with(
    client: &reqwest::Client,
    servers: &[(String, String)],
    client_id: &str,
    client_secret: &str,
    code: &str,
) -> Result<Exchanged, String> {
    let (client_id, client_secret, code) = (client_id.trim(), client_secret.trim(), code.trim());
    if client_id.is_empty() || client_secret.is_empty() || code.is_empty() {
        return Err("faltan el ID del cliente, el secreto del cliente o el código de Zoho".into());
    }
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code", code),
    ];
    let mut expired = false;
    let mut unreachable: Vec<String> = Vec::new();
    for (dc, base) in servers {
        let resp = match client
            .post(format!("{base}/oauth/v2/token"))
            .form(&form)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(dc = %dc, "zoho code exchange: {}", e.without_url());
                unreachable.push(dc.clone());
                continue;
            }
        };
        let status = resp.status().as_u16();
        let json: Value =
            serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
        let refresh = json
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty());
        let access = json
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty());
        if let (Some(r), Some(a)) = (refresh, access) {
            return Ok(Exchanged {
                data_center: dc.clone(),
                refresh_token: r.to_string(),
                access_token: a.to_string(),
                expires_in: json
                    .get("expires_in")
                    .and_then(Value::as_u64)
                    .unwrap_or(3600),
            });
        }
        let error = json.get("error").and_then(Value::as_str).unwrap_or("");
        match error {
            // Not this data centre's client: the next one.
            "invalid_client" => continue,
            // The client is here (or replicated here): the code isn't.
            "invalid_code" => expired = true,
            "invalid_client_secret" => return Err(WRONG_CLIENT_SECRET.into()),
            "Access Denied" | "access_denied" => {
                let desc = json.get("error_description").and_then(Value::as_str).unwrap_or("");
                return Err(format!("Zoho no permite pedir tokens tan seguido; probá en unos minutos ({})", super::exec::first_lines(desc, 200)));
            }
            "" if access.is_some() => return Err("Zoho no devolvió un token de actualización: generá un código nuevo y volvé a intentar".into()),
            "" => unreachable.push(format!("{dc} ({status})")),
            other => return Err(format!("Zoho rechazó el código: {}", super::exec::first_lines(other, 100))),
        }
    }
    if expired {
        return Err(CODE_EXPIRED.into());
    }
    if !unreachable.is_empty() {
        return Err(format!(
            "no se pudo consultar a Zoho en {}: revisá la conexión a internet y volvé a intentar",
            unreachable.join(", ")
        ));
    }
    Err(UNKNOWN_CLIENT.into())
}

/// `GET_LOGIN` with a just-issued access token: the passphrase's salt,
/// iterations and check value, and the user. Says plainly when the code
/// lacked Vault's scopes or the user has no Zoho Vault.
pub async fn login_details_with(
    client: &reqwest::Client,
    vault_base: &str,
    access_token: &str,
) -> Result<Value, String> {
    let ep = Endpoints {
        accounts: String::new(),
        vault: vault_base.to_string(),
    };
    match get(
        client,
        &ep,
        access_token,
        "/api/json/login?OPERATION_NAME=GET_LOGIN",
    )
    .await
    {
        Ok(json) => json
            .pointer("/operation/details")
            .cloned()
            .ok_or_else(|| "Zoho Vault no devolvió los datos de la cuenta (GET_LOGIN)".to_string()),
        Err(Some(e)) => Err(e),
        Err(None) => Err(format!(
            "Zoho Vault rechazó el token: generá el código con los permisos {SCOPES}"
        )),
    }
}

/// The user's e-mail (else their user name) in `GET_LOGIN`'s details.
pub fn identity_of(details: &Value) -> String {
    let user = details.get("USER");
    let pick = |k: &str| {
        user.and_then(|u| u.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 320 && !s.chars().any(char::is_control))
    };
    pick("EMAIL")
        .or_else(|| pick("USERNAME"))
        .unwrap_or("")
        .to_string()
}

/// Only what checks the passphrase, out of `GET_LOGIN`'s details.
pub fn passphrase_check(details: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in ["LOGIN", "SALT", "ITERATION", "PASSPHRASE"] {
        if let Some(v) = details.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// Revoke a refresh token (a connect cancelled after the exchange). Best
/// effort: Zoho keeps at most 20 per client and user anyway.
pub async fn revoke_with(
    client: &reqwest::Client,
    accounts_base: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<(), String> {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!(
        "{}:{}",
        client_id.trim(),
        client_secret.trim()
    ));
    let resp = client
        .post(format!("{accounts_base}/oauth/v2/revoke/token"))
        .header("Authorization", format!("Basic {basic}"))
        .form(&[("token", refresh_token), ("token_type", "refresh_token")])
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("Zoho respondió {}", resp.status().as_u16()))
    }
}

/// The access token of an account just connected, kept for its first use.
pub fn keep_first_token(data_center: &str, creds: &Creds, ex: &Exchanged) {
    if let Some(ep) = Endpoints::of(data_center) {
        seed_token(&ep, creds, &ex.access_token, ex.expires_in);
    }
}

// ---- API ---------------------------------------------------------------------

/// GET a Vault API path; `Err(None)` when Zoho says the access token is no
/// longer valid (the caller asks for a new one once).
async fn get(
    client: &reqwest::Client,
    ep: &Endpoints,
    token: &str,
    path_and_query: &str,
) -> Result<Value, Option<String>> {
    let resp = client
        .get(format!("{}{path_and_query}", ep.vault))
        .header("Authorization", format!("Zoho-oauthtoken {token}"))
        .send()
        .await
        .map_err(|e| {
            Some(format!(
                "no se pudo conectar con Zoho Vault ({}): {}",
                ep.vault,
                e.without_url()
            ))
        })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let result = json.pointer("/operation/result");
    let failed = result
        .and_then(|r| r.get("status"))
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("failed"));
    if failed || !(200..300).contains(&status) {
        let code = result
            .and_then(|r| r.get("error_code"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let msg = result
            .and_then(|r| r.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if matches!(code, "INVALID_OAUTHTOKEN" | "INVALID_TICKET") || status == 401 {
            return Err(None);
        }
        return Err(Some(api_error(status, code, msg)));
    }
    Ok(json)
}

/// One sentence from Zoho's failed result: its `error_code` once (Zoho
/// often repeats it as the message), its message only when it says more.
pub fn api_error(status: u16, code: &str, msg: &str) -> String {
    let code = super::exec::first_lines(code.trim(), 100);
    let msg = super::exec::first_lines(msg.trim(), 300);
    let more = !msg.is_empty() && !msg.eq_ignore_ascii_case(&code) && !msg.contains(code.as_str());
    match code.as_str() {
        "INVALID_OAUTHSCOPE" | "OAUTH_SCOPE_MISMATCH" => "el token de Zoho no tiene los permisos necesarios: generalo con ZohoVault.secrets.READ y ZohoVault.user.READ".into(),
        "NO_PERMISSION" | "PERMISSION_DENIED" if more => format!("Zoho Vault: permiso denegado ({code}): {msg}"),
        "NO_PERMISSION" | "PERMISSION_DENIED" => format!("Zoho Vault: permiso denegado ({code})"),
        // Zoho's request filter: an argument it doesn't take, or one it
        // doesn't accept that way. DBine asked wrong, not the user.
        "EXTRA_PARAM_FOUND" | "PATTERN_NOT_MATCHED" | "LESS_THAN_MIN_OCCURANCE" | "MORE_THAN_MAX_OCCURANCE" | "URL_RULE_NOT_CONFIGURED" => {
            format!("Zoho Vault rechazó el pedido ({code}): esta versión de DBine le pide algo que Zoho no acepta")
        }
        "URL_ROLLING_THROTTLES_LIMIT_EXCEEDED" | "URL_FIXED_THROTTLES_LIMIT_EXCEEDED" | "URL_RATE_LIMIT_EXCEEDED" | "TOO_MANY_REQUESTS" => {
            format!("Zoho Vault no acepta tantos pedidos seguidos; probá en unos minutos ({code})")
        }
        "" if msg.is_empty() => format!("Zoho Vault respondió {status}"),
        "" => format!("Zoho Vault respondió {status}: {msg}"),
        _ if more => format!("Zoho Vault rechazó el pedido ({code}): {msg}"),
        _ => format!("Zoho Vault rechazó el pedido ({code})"),
    }
}

/// GET with the access token, asking for a new one once if Zoho says it's
/// no longer valid.
async fn api(
    client: &reqwest::Client,
    ep: &Endpoints,
    creds: &Creds,
    path_and_query: &str,
) -> Result<Value, String> {
    for attempt in 0..2 {
        let token = access_token(client, ep, creds).await?;
        match get(client, ep, &token, path_and_query).await {
            Ok(v) => return Ok(v),
            Err(Some(e)) => return Err(e),
            Err(None) if attempt == 0 => forget_token(&token_cache_key(ep, creds)),
            Err(None) => {}
        }
    }
    Err("Zoho Vault rechazó el token de acceso (¿el cliente tiene acceso a Zoho Vault en este centro de datos?)".into())
}

/// `(value, username)` of the secret, talking to `ep`.
pub async fn read_with(
    client: &reqwest::Client,
    ep: &Endpoints,
    req: &Request<'_>,
    creds: &Creds,
) -> Result<(String, Option<String>), String> {
    let login = api(
        client,
        ep,
        creds,
        "/api/json/login?OPERATION_NAME=GET_LOGIN",
    )
    .await?;
    let details = login
        .pointer("/operation/details")
        .ok_or("Zoho Vault no devolvió los datos de la cuenta (GET_LOGIN)")?;
    let mut master = master_key(details, &creds.passphrase)?;
    let secret = find_secret(client, ep, creds, req.secret).await;
    let out = match secret {
        Ok(secret) => {
            let shared = secret
                .get("isshared")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("YES"));
            let key = if shared {
                org_key(client, ep, creds, &master).await
            } else {
                Ok(master.clone())
            };
            key.and_then(|mut key| {
                let r = values(&secret, &key, req);
                wipe(&mut key);
                r
            })
        }
        Err(e) => Err(e),
    };
    wipe(&mut master);
    out
}

/// The master key from `GET_LOGIN`'s details, checked against the account's
/// check value when there is one.
pub fn master_key(details: &Value, passphrase: &str) -> Result<String, String> {
    if passphrase.is_empty() {
        return Err("falta la frase de contraseña maestra de Zoho Vault".into());
    }
    let salt = details.get("SALT").and_then(Value::as_str).unwrap_or("");
    let check = details
        .get("PASSPHRASE")
        .and_then(Value::as_str)
        .unwrap_or("");
    let kind = details
        .get("LOGIN")
        .and_then(Value::as_str)
        .unwrap_or("PBKDF2_AES");
    if kind.eq_ignore_ascii_case("SHA256") {
        let key = super::cloud_auth::hex(&Sha256::digest(passphrase.as_bytes()));
        // Older accounts: the check is SHA-256 of the passphrase and salt.
        if check.len() == 64 && check.chars().all(|c| c.is_ascii_hexdigit()) {
            let auth =
                super::cloud_auth::hex(&Sha256::digest(format!("{passphrase}{salt}").as_bytes()));
            if !auth.eq_ignore_ascii_case(check) {
                return Err(WRONG_PASSPHRASE.into());
            }
        }
        return Ok(key);
    }
    if salt.is_empty() {
        return Err("Zoho Vault no devolvió la sal de la cuenta (GET_LOGIN)".into());
    }
    let iterations = details
        .get("ITERATION")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
        .unwrap_or(1000);
    if !(1..=10_000_000).contains(&iterations) {
        return Err(format!(
            "Zoho Vault devolvió una cantidad de iteraciones fuera de rango ({iterations})"
        ));
    }
    let key = pbkdf2_key(passphrase, salt, iterations as u32);
    // The check value decrypts to a small JSON object with this key only.
    if !check.is_empty() {
        let ok = decrypt(check, &key)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .is_some_and(|v| v.is_object());
        if !ok {
            return Err(WRONG_PASSPHRASE.into());
        }
    }
    Ok(key)
}

/// PBKDF2-HMAC-SHA256 over the passphrase and the salt (as text), 32 bytes,
/// as lowercase hex: the "password" the values are encrypted with.
pub fn pbkdf2_key(passphrase: &str, salt: &str, iterations: u32) -> String {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), salt.as_bytes(), iterations, &mut out);
    let key = super::cloud_auth::hex(&out);
    out.iter_mut().for_each(|b| *b = 0);
    key
}

/// Secrets one list page asks for (`rowPerPage`).
const PAGE: usize = 100;
/// Pages read at most: one past the browse limit, so a cut list is known.
const MAX_PAGES: usize = super::browse::MAX_ENTRIES / PAGE + 1;

/// Every secret the user sees, page by page, each handed to `each` once.
/// Only the list's documented paging arguments go (`isAsc`, `pageNum`,
/// `rowPerPage`): Zoho refuses any other with `EXTRA_PARAM_FOUND`, and
/// `passwordName`, although documented, is refused that way too. It stops
/// on a short page, on the second page in a row with nothing new (one is
/// skipped: if `pageNum` starts at 1, 0 may repeat it) or after
/// [`MAX_PAGES`]; `Ok(true)` when it was cut there.
async fn each_secret(
    client: &reqwest::Client,
    ep: &Endpoints,
    creds: &Creds,
    mut each: impl FnMut(Value),
) -> Result<bool, String> {
    let mut seen = std::collections::HashSet::new();
    let mut stale = 0;
    for page in 0..MAX_PAGES {
        let json = api(
            client,
            ep,
            creds,
            &format!("/api/rest/json/v1/secrets?isAsc=true&pageNum={page}&rowPerPage={PAGE}"),
        )
        .await?;
        let list = match json.pointer("/operation/Details") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let n = list.len();
        let mut new = 0;
        for s in list {
            let id = id_of(&s);
            if id.is_empty() || seen.insert(id) {
                new += 1;
                each(s);
            }
        }
        stale = if new == 0 { stale + 1 } else { 0 };
        if n < PAGE || stale == 2 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The secret's JSON: by ID (digits), else by name, looked for in the list
/// (Zoho has no name filter it accepts): exactly, else ignoring case.
async fn find_secret(
    client: &reqwest::Client,
    ep: &Endpoints,
    creds: &Creds,
    wanted: &str,
) -> Result<Value, String> {
    let wanted = wanted.trim();
    if !wanted.is_empty() && wanted.chars().all(|c| c.is_ascii_digit()) {
        let json = api(
            client,
            ep,
            creds,
            &format!("/api/rest/json/v1/secrets/{wanted}"),
        )
        .await?;
        return first_detail(&json)
            .ok_or_else(|| format!("Zoho Vault: no existe el secreto con el ID {wanted}"));
    }
    let lower = wanted.to_lowercase();
    // The ones with that name, whole; the similar ones, by name only (for
    // the message), not their ciphertext.
    let mut kept: Vec<Value> = Vec::new();
    let mut similar = 0;
    let cut = each_secret(client, ep, creds, |s| {
        let name = name_of(&s).to_lowercase();
        if name == lower {
            kept.push(s);
        } else if similar < 8
            && !name.is_empty()
            && !lower.is_empty()
            && (name.contains(&lower) || lower.contains(&name))
        {
            similar += 1;
            kept.push(serde_json::json!({ "secretname": name_of(&s) }));
        }
    })
    .await?;
    if cut && !kept.iter().any(|s| name_of(s).to_lowercase() == lower) {
        return Err(format!(
            "Zoho Vault: no hay un secreto «{wanted}» entre los primeros {} (indicá su ID)",
            super::browse::MAX_ENTRIES
        ));
    }
    pick_by_name(kept, wanted)
}

fn first_detail(json: &Value) -> Option<Value> {
    match json.pointer("/operation/Details")? {
        Value::Array(a) => a.first().cloned(),
        v @ Value::Object(_) => Some(v.clone()),
        _ => None,
    }
}

fn name_of(v: &Value) -> &str {
    v.get("secretname").and_then(Value::as_str).unwrap_or("")
}

fn id_of(v: &Value) -> String {
    match v.get("secretid") {
        Some(Value::String(s)) => s.clone(),
        Some(v @ Value::Number(_)) => v.to_string(),
        _ => String::new(),
    }
}

/// The one secret named `wanted` (exactly, else ignoring case) among the
/// search's results.
pub fn pick_by_name(list: Vec<Value>, wanted: &str) -> Result<Value, String> {
    let exact: Vec<&Value> = list.iter().filter(|s| name_of(s) == wanted).collect();
    let lower = wanted.to_lowercase();
    let found = if exact.is_empty() {
        list.iter()
            .filter(|s| name_of(s).to_lowercase() == lower)
            .collect()
    } else {
        exact
    };
    match found.len() {
        1 => Ok(found[0].clone()),
        0 => {
            let mut names: Vec<&str> = list
                .iter()
                .map(name_of)
                .filter(|n| !n.is_empty())
                .take(8)
                .collect();
            names.sort_unstable();
            names.dedup();
            if names.is_empty() {
                Err(format!(
                    "Zoho Vault: no hay un secreto «{wanted}» (o el usuario no tiene acceso)"
                ))
            } else {
                Err(format!(
                    "Zoho Vault: no hay un secreto «{wanted}» (parecidos: {})",
                    names.join(", ")
                ))
            }
        }
        n => {
            let ids: Vec<String> = found.iter().take(6).map(|s| id_of(s)).collect();
            Err(format!("Zoho Vault tiene {n} secretos «{wanted}» (IDs: {}): indicá el ID del que corresponde", ids.join(", ")))
        }
    }
}

/// The organization key (shared secrets): the user's private key decrypted
/// with the master key, then the RSA-encrypted shared key with it.
async fn org_key(
    client: &reqwest::Client,
    ep: &Endpoints,
    creds: &Creds,
    master: &str,
) -> Result<String, String> {
    let json = api(client, ep, creds, "/api/rest/json/v1/user/orgkey").await?;
    let d = first_detail(&json).ok_or("Zoho Vault no devolvió la clave de la organización")?;
    let private = d
        .get("privateKey")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && *s != "null");
    let shared = d
        .get("sharedKey")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && *s != "null");
    let kind = d.get("rsaKeyType").and_then(Value::as_str).unwrap_or("");
    match (private, shared) {
        (Some(p), Some(s)) => decrypt_org_key(p, s, kind, master),
        _ => Err("el secreto es compartido, pero Zoho Vault no devolvió la clave de la organización para este usuario".into()),
    }
}

/// The master passphrase opened the account but not the organization's
/// private key.
pub const ORG_PASSPHRASE: &str =
    "la frase de contraseña maestra no abre la clave privada de la organización de Zoho Vault";
/// The private key decrypted, but isn't any format Zoho's clients write.
pub const ORG_PRIVATE_FORMAT: &str =
    "la clave privada de la organización de Zoho Vault tiene un formato que DBine no reconoce";
/// The RSA-encrypted organization key isn't hex or base64 as expected.
pub const ORG_SHARED_FORMAT: &str =
    "la clave de la organización de Zoho Vault viene cifrada en un formato que DBine no reconoce";
/// RSA refused the shared key with that private key.
pub const ORG_RSA: &str =
    "la clave de la organización de Zoho Vault no se pudo descifrar con la clave privada";

/// The organization key from `/user/orgkey`'s `privateKey` and `sharedKey`.
/// Zoho's clients (the browser extension's VaultLogin.decryptOrgKey and
/// BgZCrypt) write two kinds, told apart here by the decrypted private key
/// (`rsaKeyType`, `RSA_1024` when absent, only goes to the log):
///
/// - `RSA_1024`: the private key is the text `n,e,d,p,q,dmp1,dmq1,coeff`
///   in hex (jsbn's `RSAKey.setPrivateEx`), the shared key RSA PKCS#1 v1.5
///   ciphertext in hex (`RSAKey.decrypt`).
/// - `RSA_4096`: the private key is base64 PKCS#8, the shared key base64
///   RSA-OAEP with SHA-256 (WebCrypto's `RSA-OAEP`, hash `SHA-256`).
///
/// Each stage fails with its own message, and the log says where it stopped
/// with lengths and shapes only: never a key, a value or plaintext.
pub fn decrypt_org_key(
    private_key: &str,
    shared_key: &str,
    key_type: &str,
    master: &str,
) -> Result<String, String> {
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::traits::PublicKeyParts;
    // Zoho's label, not the text Zoho sent (it goes to the log).
    let kind = match key_type {
        "" => "(none: RSA_1024)",
        "RSA_1024" => "RSA_1024",
        "RSA_4096" => "RSA_4096",
        _ => "(another)",
    };
    let stop = |stage: &str, msg: &str| {
        tracing::warn!(
            rsa_key_type = kind,
            private_key_len = private_key.len(),
            shared_key_len = shared_key.len(),
            "zoho org key: {stage}"
        );
        msg.to_string()
    };
    let Ok(mut bytes) = decrypt(private_key, master) else {
        return Err(stop(
            "privateKey is not base64 of a nonce and ciphertext",
            ORG_PRIVATE_FORMAT,
        ));
    };
    // The key text is ASCII either way; anything else is another key's garbage.
    if !bytes
        .iter()
        .all(|b| b.is_ascii_graphic() || b.is_ascii_whitespace())
    {
        let n = bytes.len();
        wipe_bytes(&mut bytes);
        return Err(stop(&format!("privateKey decrypted to {n} bytes that are not text: the master key does not open it"), ORG_PASSPHRASE));
    }
    let mut text = String::from_utf8(bytes).unwrap_or_default();
    let parts = text.trim().split(',').count();
    let key = if parts > 1 {
        let key = jsbn_private_key(&text);
        let shape = format!(
            "privateKey decrypted: {} chars, comma-separated: {parts} parts",
            text.trim().len()
        );
        wipe(&mut text);
        let Some(key) = key else {
            return Err(stop(
                &format!("{shape}, not 8 hex parts of an RSA key"),
                ORG_PRIVATE_FORMAT,
            ));
        };
        tracing::debug!("zoho org key: {shape} (jsbn RSAKey)");
        let Some(mut ct) = unhex(shared_key.trim()) else {
            return Err(stop("sharedKey is not hex (RSA_1024)", ORG_SHARED_FORMAT));
        };
        let plain = left_pad(&ct, key.size())
            .and_then(|c| super::rsa_lc::decrypt(&key, super::rsa_lc::Padding::Pkcs1v15, &c));
        wipe_bytes(&mut ct);
        plain.ok_or_else(|| {
            stop(
                &format!(
                    "sharedKey: {} hex chars, RSA PKCS#1 v1.5 refused it",
                    shared_key.trim().len()
                ),
                ORG_RSA,
            )
        })?
    } else {
        let der = base64::engine::general_purpose::STANDARD.decode(text.trim().as_bytes());
        let shape = format!(
            "privateKey decrypted: {} chars, without commas",
            text.trim().len()
        );
        wipe(&mut text);
        let Ok(mut der) = der else {
            return Err(stop(&format!("{shape}, not base64"), ORG_PRIVATE_FORMAT));
        };
        let key = rsa::RsaPrivateKey::from_pkcs8_der(&der);
        let der_len = der.len();
        wipe_bytes(&mut der);
        let Ok(key) = key else {
            return Err(stop(
                &format!("{shape}, base64 of {der_len} bytes that are not a PKCS#8 RSA key"),
                ORG_PRIVATE_FORMAT,
            ));
        };
        tracing::debug!("zoho org key: {shape} (PKCS#8, {der_len} bytes)");
        let Ok(mut ct) =
            base64::engine::general_purpose::STANDARD.decode(shared_key.trim().as_bytes())
        else {
            return Err(stop(
                "sharedKey is not base64 (RSA_4096)",
                ORG_SHARED_FORMAT,
            ));
        };
        let plain = left_pad(&ct, key.size())
            .and_then(|c| super::rsa_lc::decrypt(&key, super::rsa_lc::Padding::OaepSha256, &c));
        let ct_len = ct.len();
        wipe_bytes(&mut ct);
        plain.ok_or_else(|| {
            stop(
                &format!("sharedKey: base64 of {ct_len} bytes, RSA-OAEP SHA-256 refused it"),
                ORG_RSA,
            )
        })?
    };
    String::from_utf8(key).map_err(|e| {
        wipe_bytes(&mut e.into_bytes());
        stop("sharedKey decrypted to bytes that are not UTF-8", ORG_RSA)
    })
}

/// jsbn's `setPrivateEx(n, e, d, p, q, dmp1, dmq1, coeff)` from its hex
/// text. Like jsbn's CRT decryption, only `p`, `q` and `e` make the key
/// (`n` must agree); the rest is derived again.
fn jsbn_private_key(text: &str) -> Option<rsa::RsaPrivateKey> {
    use rsa::traits::PublicKeyParts;
    let mut parts: Vec<Vec<u8>> = text
        .trim()
        .split(',')
        .map(|p| unhex(p.trim()))
        .collect::<Option<_>>()?;
    let trim = |b: &[u8]| b[b.iter().position(|x| *x != 0).unwrap_or(b.len())..].to_vec();
    let key = (parts.len() == 8).then(|| {
        let e = rsa::BoxedUint::from_be_slice_vartime(&parts[1]);
        let (p, q) = (
            rsa::BoxedUint::from_be_slice_vartime(&parts[3]),
            rsa::BoxedUint::from_be_slice_vartime(&parts[4]),
        );
        rsa::RsaPrivateKey::from_p_q(p, q, e)
            .ok()
            .filter(|k| trim(&k.n().as_ref().to_be_bytes()) == trim(&parts[0]))
    });
    parts.iter_mut().for_each(wipe_bytes);
    key.flatten()
}

/// Hex digits (either case; an odd count gets a leading zero, as jsbn
/// writes numbers) as bytes.
fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let padded = if s.len() % 2 == 1 {
        format!("0{s}")
    } else {
        s.to_string()
    };
    padded
        .as_bytes()
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok())
        .collect()
}

/// The ciphertext as the modulus' length: jsbn drops leading zero bytes.
fn left_pad(ct: &[u8], size: usize) -> Option<Vec<u8>> {
    let start = ct.iter().position(|b| *b != 0).unwrap_or(ct.len());
    let ct = &ct[start..];
    (ct.len() <= size).then(|| {
        let mut out = vec![0u8; size - ct.len()];
        out.extend_from_slice(ct);
        out
    })
}

/// The secret's data: `secretData` (a JSON object, or its text) plus the
/// custom fields (`customcolumn`: base64 of `{"customcol":[{colname,value}]}`).
fn fields_of(secret: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let data = secret
        .get("secretData")
        .or_else(|| secret.get("SECRETDATA"));
    let obj = match data {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok(),
        Some(v @ Value::Object(_)) => Some(v.clone()),
        _ => None,
    };
    if let Some(Value::Object(m)) = obj {
        for (k, v) in m {
            if let Some(s) = v.as_str() {
                out.push((k, s.to_string()));
            }
        }
    }
    let custom = secret
        .get("customcolumn")
        .or_else(|| secret.get("CUSTOMCOLUMN"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if let Some(c) = custom {
        let parsed = base64::engine::general_purpose::STANDARD
            .decode(c.trim().as_bytes())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        if let Some(cols) = parsed
            .as_ref()
            .and_then(|p| p.get("customcol"))
            .and_then(Value::as_array)
        {
            for col in cols {
                if let (Some(n), Some(v)) = (
                    col.get("colname").and_then(Value::as_str),
                    col.get("value").and_then(Value::as_str),
                ) {
                    out.push((n.to_string(), v.to_string()));
                }
            }
        }
    }
    out
}

/// Decrypt one field's value; empty stays empty.
fn open_value(cipher: &str, key: &str, field: &str, secret: &str) -> Result<String, String> {
    if cipher.is_empty() {
        return Ok(String::new());
    }
    decrypt(cipher, key)
        .ok()
        .and_then(|b| String::from_utf8(b).map_err(|e| wipe_bytes(&mut e.into_bytes())).ok())
        .ok_or_else(|| format!("no se pudo descifrar el campo «{field}» del secreto «{secret}» de Zoho Vault (¿la frase de contraseña maestra es la correcta?)"))
}

pub fn values(
    secret: &Value,
    key: &str,
    req: &Request<'_>,
) -> Result<(String, Option<String>), String> {
    let fields = fields_of(secret);
    let name = Some(name_of(secret))
        .filter(|n| !n.is_empty())
        .unwrap_or(req.secret.trim())
        .to_string();
    let want = Some(req.attribute.trim())
        .filter(|a| !a.is_empty())
        .unwrap_or("password");
    let find = |w: &str| {
        fields
            .iter()
            .find(|(k, _)| k == w)
            .or_else(|| fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(w)))
    };
    let Some((k, c)) = find(want) else {
        let mut names: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).take(30).collect();
        names.sort_unstable();
        return Err(format!(
            "el secreto «{name}» de Zoho Vault no tiene el campo «{want}» (tiene: {})",
            names.join(", ")
        ));
    };
    let value = open_value(c, key, k, &name)?;
    let user_field = Some(req.user_field.trim())
        .filter(|f| !f.is_empty())
        .unwrap_or("username");
    let username = if req.username {
        match find(user_field) {
            Some((k, c)) => Some(open_value(c, key, k, &name)?).filter(|u| !u.is_empty()),
            None => None,
        }
    } else {
        None
    };
    if req.username && username.is_none() {
        return Err(format!(
            "el secreto «{name}» de Zoho Vault no tiene usuario ({user_field})"
        ));
    }
    Ok((value, username))
}

// ---- browse ------------------------------------------------------------------

/// The secrets' names (paged), or a secret's field names (`item`, an ID or
/// a name). Neither is encrypted: no passphrase, nothing decrypted.
pub async fn browse(
    data_center: &str,
    creds: &Creds,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    let ep = Endpoints::of(data_center).ok_or_else(|| {
        format!(
            "el centro de datos de Zoho «{}» no es válido",
            data_center.trim()
        )
    })?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    browse_with(&client, &ep, creds, at).await
}

pub async fn browse_with(
    client: &reqwest::Client,
    ep: &Endpoints,
    creds: &Creds,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    use super::browse::{unique_names, wipe_json, Entry, Listing};
    if let Some(item) = at.item {
        let mut secret = find_secret(client, ep, creds, item).await?;
        let mut names: Vec<String> = fields_of(&secret)
            .into_iter()
            .map(|(mut k, mut v)| {
                wipe(&mut v);
                std::mem::take(&mut k)
            })
            .collect();
        wipe_json(&mut secret);
        names.dedup();
        return Ok(Listing::fields(names));
    }
    // Names and IDs only; the rest (ciphertext) isn't kept.
    let mut all: Vec<Value> = Vec::new();
    let cut = each_secret(client, ep, creds, |s| {
        all.push(serde_json::json!({ "secretname": name_of(&s), "secretid": id_of(&s) }))
    })
    .await?;
    let unique = unique_names(all.iter().map(name_of));
    let mut out: Vec<Entry> = all
        .iter()
        .map(|s| {
            let (name, id) = (name_of(s).to_string(), id_of(s));
            let by_name = !name.trim().is_empty()
                && !name.chars().all(|c| c.is_ascii_digit())
                && unique.contains(&name);
            Entry::item(
                if by_name { name.clone() } else { id.clone() },
                if name.is_empty() { id.clone() } else { name },
                "",
                id,
            )
        })
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    let mut listing = Listing::complete(out);
    listing.truncated |= cut;
    Ok(listing)
}

// ---- Zoho's AES-CTR ------------------------------------------------------------

/// The AES-256 key of a key text: its first 32 bytes (zero-padded) as an
/// AES-256 key encrypting its own first 16 bytes; that block, twice.
fn cipher_for(password: &str) -> Aes256 {
    let mut pw = [0u8; 32];
    let src = password.as_bytes();
    let n = src.len().min(32);
    pw[..n].copy_from_slice(&src[..n]);
    let first = Aes256::new(GenericArray::from_slice(&pw));
    let mut block = GenericArray::clone_from_slice(&pw[..16]);
    first.encrypt_block(&mut block);
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(&block);
    key[16..].copy_from_slice(&block);
    let c = Aes256::new(GenericArray::from_slice(&key));
    pw.iter_mut().chain(key.iter_mut()).for_each(|b| *b = 0);
    c
}

fn ctr(cipher: &Aes256, nonce: &[u8], data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(16).enumerate() {
        let mut counter = [0u8; 16];
        counter[..8].copy_from_slice(nonce);
        counter[8..].copy_from_slice(&(i as u64).to_be_bytes());
        let mut block = GenericArray::from(counter);
        cipher.encrypt_block(&mut block);
        for (b, k) in chunk.iter_mut().zip(block.iter()) {
            *b ^= k;
        }
    }
}

/// A value's bytes: base64 of the 8-byte nonce and the ciphertext.
pub fn decrypt(b64: &str, key: &str) -> Result<Vec<u8>, ()> {
    let mut data = base64::engine::general_purpose::STANDARD
        .decode(b64.trim().as_bytes())
        .map_err(|_| ())?;
    if data.len() < 8 {
        wipe_bytes(&mut data);
        return Err(());
    }
    let cipher = cipher_for(key);
    let (nonce, body) = data.split_at_mut(8);
    ctr(&cipher, nonce, body);
    let out = body.to_vec();
    wipe_bytes(&mut data);
    Ok(out)
}

/// The inverse, with the given nonce (tests build a mock account with it).
#[cfg(test)]
pub fn encrypt(plain: &str, key: &str, nonce: [u8; 8]) -> String {
    let mut data = nonce.to_vec();
    data.extend_from_slice(plain.as_bytes());
    let cipher = cipher_for(key);
    let (n, body) = data.split_at_mut(8);
    ctr(&cipher, n, body);
    base64::engine::general_purpose::STANDARD.encode(&data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Made with Zoho Vault's own web crypto (zohovault.js), Node and
    /// OpenSSL: testdata/zoho-vectors.mjs.
    fn vectors() -> Value {
        serde_json::from_str(include_str!("testdata/zoho-vectors.json")).unwrap()
    }

    fn s<'a>(v: &'a Value, k: &str) -> &'a str {
        v[k].as_str().unwrap()
    }

    #[test]
    fn key_derivation_vectors() {
        let v = vectors();
        assert_eq!(
            pbkdf2_key(
                s(&v, "passphrase"),
                s(&v, "salt"),
                v["iterations"].as_u64().unwrap() as u32
            ),
            s(&v, "master")
        );
        let details = json!({ "LOGIN": "PBKDF2_AES", "SALT": s(&v, "salt"), "ITERATION": v["iterations"], "PASSPHRASE": s(&v, "passphrase_check") });
        assert_eq!(
            master_key(&details, s(&v, "passphrase")).unwrap(),
            s(&v, "master")
        );
        assert_eq!(
            master_key(&details, "dummy-passphrase-n").unwrap_err(),
            WRONG_PASSPHRASE
        );
        // The iteration count may come as text.
        let as_text = json!({ "SALT": s(&v, "salt"), "ITERATION": "1000", "PASSPHRASE": s(&v, "passphrase_check") });
        assert_eq!(
            master_key(&as_text, s(&v, "passphrase")).unwrap(),
            s(&v, "master")
        );
        // Older accounts: SHA-256.
        let legacy = json!({ "LOGIN": "SHA256", "SALT": s(&v, "salt"), "PASSPHRASE": s(&v, "legacy_passauth") });
        assert_eq!(
            master_key(&legacy, s(&v, "legacy_passphrase")).unwrap(),
            s(&v, "legacy_master")
        );
        assert_eq!(master_key(&legacy, "other").unwrap_err(), WRONG_PASSPHRASE);
        assert!(master_key(&details, "").is_err());
    }

    #[test]
    fn aes_ctr_vectors() {
        let v = vectors();
        for a in v["aes"].as_array().unwrap() {
            let (key, plain, cipher) = (s(a, "key"), s(a, "plain"), s(a, "cipher"));
            assert_eq!(
                String::from_utf8(decrypt(cipher, key).unwrap()).unwrap(),
                plain,
                "{plain}"
            );
            // Encrypting with the same nonce gives the same bytes.
            let raw = base64::engine::general_purpose::STANDARD
                .decode(cipher)
                .unwrap();
            assert_eq!(
                encrypt(plain, key, raw[..8].try_into().unwrap()),
                cipher,
                "{plain}"
            );
        }
        // Another key gives other bytes (and, here, not even UTF-8 text).
        let a = &v["aes"][2];
        assert_ne!(
            decrypt(
                s(a, "cipher"),
                "0000000000000000000000000000000000000000000000000000000000000000"
            )
            .unwrap(),
            s(a, "plain").as_bytes()
        );
        assert!(decrypt("not base64!", "k").is_err());
        assert!(decrypt("AAAA", "k").is_err(), "shorter than the nonce");
    }

    #[test]
    fn organization_key_vectors() {
        let v = vectors();
        let (master, org) = (s(&v, "master"), s(&v, "org_key"));
        // RSA_1024 (jsbn: hex components, hex PKCS#1 v1.5), with or without
        // rsaKeyType: the data says which.
        assert_eq!(
            decrypt_org_key(
                s(&v, "rsa1024_private_key"),
                s(&v, "rsa1024_shared_key"),
                "",
                master
            )
            .unwrap(),
            org
        );
        assert_eq!(
            decrypt_org_key(
                s(&v, "rsa1024_private_key"),
                s(&v, "rsa1024_shared_key"),
                "RSA_1024",
                master
            )
            .unwrap(),
            org
        );
        assert_eq!(
            decrypt_org_key(
                s(&v, "rsa1024_private_key"),
                &s(&v, "rsa1024_shared_key").to_uppercase(),
                "",
                master
            )
            .unwrap(),
            org,
            "hex in either case"
        );
        // RSA_4096 (WebCrypto: base64 PKCS#8, base64 OAEP SHA-256).
        assert_eq!(
            decrypt_org_key(
                s(&v, "rsa4096_private_key"),
                s(&v, "rsa4096_shared_key"),
                "RSA_4096",
                master
            )
            .unwrap(),
            org
        );
        // The organization key opens the shared secrets.
        let shared = secret("org-db", "2", true, s(&v, "shared_secret_data"));
        let key = decrypt_org_key(
            s(&v, "rsa1024_private_key"),
            s(&v, "rsa1024_shared_key"),
            "",
            master,
        )
        .unwrap();
        let r = Request {
            data_center: "eu",
            secret: "org-db",
            attribute: "",
            username: true,
            user_field: "",
        };
        assert_eq!(
            values(&shared, &key, &r).unwrap(),
            ("org-secret-9".to_string(), Some("org_user".to_string()))
        );
    }

    /// Each stage of the organization key fails with its own message, and
    /// none carries a key, a value or plaintext.
    #[test]
    fn organization_key_stages() {
        // `cargo test organization_key_stages -- --nocapture` shows the log lines.
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();
        let v = vectors();
        let (master, org) = (s(&v, "master"), s(&v, "org_key"));
        let (p1024, s1024) = (s(&v, "rsa1024_private_key"), s(&v, "rsa1024_shared_key"));
        let (p4096, s4096) = (s(&v, "rsa4096_private_key"), s(&v, "rsa4096_shared_key"));
        let fails = |p: &str, sk: &str, m: &str| decrypt_org_key(p, sk, "", m).unwrap_err();
        // The master key doesn't open the private key.
        assert_eq!(fails(p1024, s1024, s(&v, "legacy_master")), ORG_PASSPHRASE);
        assert_eq!(fails(p4096, s4096, s(&v, "legacy_master")), ORG_PASSPHRASE);
        // The private key isn't any format Zoho writes.
        assert_eq!(fails("not base64!", s1024, master), ORG_PRIVATE_FORMAT);
        assert_eq!(
            fails(&encrypt("a1,b2,c3", master, [1; 8]), s1024, master),
            ORG_PRIVATE_FORMAT,
            "too few parts"
        );
        assert_eq!(
            fails(
                &encrypt("a1,b2,c3,d4,e5,f6,a7,zz", master, [1; 8]),
                s1024,
                master
            ),
            ORG_PRIVATE_FORMAT,
            "not hex"
        );
        assert_eq!(
            fails(
                &encrypt("a1,10001,c3,d5,e5,f7,a7,b9", master, [1; 8]),
                s1024,
                master
            ),
            ORG_PRIVATE_FORMAT,
            "not an RSA key"
        );
        assert_eq!(
            fails(&encrypt("!!no-commas!!", master, [1; 8]), s4096, master),
            ORG_PRIVATE_FORMAT
        );
        assert_eq!(
            fails(&encrypt("AAAA", master, [1; 8]), s4096, master),
            ORG_PRIVATE_FORMAT,
            "base64, not PKCS#8"
        );
        // The shared key isn't the encoding that key type uses.
        assert_eq!(
            fails(p1024, s4096, master),
            ORG_SHARED_FORMAT,
            "base64 for a jsbn key"
        );
        assert_eq!(fails(p4096, "%%%", master), ORG_SHARED_FORMAT);
        // RSA refuses it: encrypted for another key, or tampered with.
        let other = format!(
            "{}{}",
            &s1024[..10],
            if &s1024[10..11] == "0" { "1" } else { "0" }
        ) + &s1024[11..];
        assert_eq!(fails(p1024, &other, master), ORG_RSA);
        assert_eq!(
            fails(
                p4096,
                &base64::engine::general_purpose::STANDARD.encode([7u8; 256]),
                master
            ),
            ORG_RSA
        );
        assert_eq!(
            fails(p1024, &"ff".repeat(200), master),
            ORG_RSA,
            "longer than the modulus"
        );
        for m in [
            ORG_PASSPHRASE,
            ORG_PRIVATE_FORMAT,
            ORG_SHARED_FORMAT,
            ORG_RSA,
        ] {
            assert!(!m.contains(org) && !m.contains(master), "{m}");
        }
    }

    fn secret(name: &str, id: &str, shared: bool, data: &str) -> Value {
        json!({ "secretid": id, "secretname": name, "isshared": if shared { "YES" } else { "NO" }, "secretData": data })
    }

    #[test]
    fn fields_and_usernames() {
        let v = vectors();
        let personal = secret("pg-main", "1", false, s(&v, "personal_secret_data"));
        let r = |attr: &'static str, username: bool| Request {
            data_center: "eu",
            secret: "pg-main",
            attribute: attr,
            username,
            user_field: "",
        };
        assert_eq!(
            values(&personal, s(&v, "master"), &r("", true)).unwrap(),
            ("pg-secret-1".to_string(), Some("app_user".to_string()))
        );
        assert_eq!(
            values(&personal, s(&v, "master"), &r("username", false))
                .unwrap()
                .0,
            "app_user"
        );
        assert_eq!(
            values(&personal, s(&v, "master"), &r("PASSWORD", false))
                .unwrap()
                .0,
            "pg-secret-1",
            "case doesn't matter"
        );
        let e = values(&personal, s(&v, "master"), &r("pin", false)).unwrap_err();
        assert!(
            e.contains("no tiene el campo «pin»")
                && e.contains("password")
                && e.contains("username")
                && !e.contains("pg-secret"),
            "{e}"
        );
        let e = values(&personal, s(&v, "legacy_master"), &r("", false)).unwrap_err();
        assert!(
            e.contains("no se pudo descifrar"),
            "a wrong key never shows garbage: {e}"
        );
        let mut shared = secret("org-db", "2", true, s(&v, "shared_secret_data"));
        shared["customcolumn"] = json!(s(&v, "shared_custom_column"));
        assert_eq!(
            values(&shared, s(&v, "org_key"), &r("Port", false))
                .unwrap()
                .0,
            "5432",
            "a custom field"
        );
        assert_eq!(
            values(&shared, s(&v, "org_key"), &r("", true)).unwrap(),
            ("org-secret-9".to_string(), Some("org_user".to_string()))
        );
    }

    #[test]
    fn names_and_ambiguity() {
        let list = vec![
            secret("pg-main", "11", false, "{}"),
            secret("pg-main-old", "12", false, "{}"),
            secret("PG-MAIN", "13", false, "{}"),
        ];
        assert_eq!(
            id_of(&pick_by_name(list.clone(), "pg-main").unwrap()),
            "11",
            "exact first"
        );
        assert_eq!(
            id_of(&pick_by_name(list.clone(), "pg-main-OLD").unwrap()),
            "12",
            "else ignoring case"
        );
        let twice = vec![
            secret("db", "21", false, "{}"),
            secret("db", "22", true, "{}"),
        ];
        let e = pick_by_name(twice, "db").unwrap_err();
        assert!(
            e.contains("2 secretos «db»") && e.contains("21") && e.contains("22"),
            "{e}"
        );
        let e = pick_by_name(list, "mysql").unwrap_err();
        assert!(
            e.contains("no hay un secreto «mysql»") && e.contains("pg-main"),
            "{e}"
        );
        assert!(pick_by_name(vec![], "x")
            .unwrap_err()
            .contains("no tiene acceso"));
    }

    #[test]
    fn errors() {
        assert!(oauth_error(200, "invalid_client", "").contains("ID o el secreto del cliente"));
        assert!(oauth_error(200, "invalid_code", "").contains("token de actualización"));
        assert!(oauth_error(
            400,
            "Access Denied",
            "You have made too many requests continuously."
        )
        .contains("tan seguido"));
        assert_eq!(
            oauth_error(500, "", ""),
            "Zoho respondió 500 al pedir el token de acceso"
        );
        assert!(api_error(200, "INVALID_OAUTHSCOPE", "").contains("ZohoVault.secrets.READ"));
        assert_eq!(api_error(500, "", ""), "Zoho Vault respondió 500");
        assert_eq!(
            api_error(200, "SOMETHING", "went wrong"),
            "Zoho Vault rechazó el pedido (SOMETHING): went wrong"
        );
        assert_eq!(
            api_error(200, "SECRET_NOT_FOUND", "SECRET_NOT_FOUND"),
            "Zoho Vault rechazó el pedido (SECRET_NOT_FOUND)",
            "the code once"
        );
        let e = api_error(400, "EXTRA_PARAM_FOUND", "EXTRA_PARAM_FOUND");
        assert!(
            e.starts_with("Zoho Vault rechazó el pedido (EXTRA_PARAM_FOUND)")
                && e.matches("EXTRA_PARAM_FOUND").count() == 1,
            "{e}"
        );
        assert!(api_error(200, "URL_ROLLING_THROTTLES_LIMIT_EXCEEDED", "")
            .contains("probá en unos minutos"));
        assert_eq!(
            api_error(200, "NO_PERMISSION", "no access to this secret"),
            "Zoho Vault: permiso denegado (NO_PERMISSION): no access to this secret"
        );
    }

    #[test]
    fn only_the_data_centres_hosts() {
        let ep = Endpoints::of("eu").unwrap();
        assert_eq!(
            (ep.accounts.as_str(), ep.vault.as_str()),
            ("https://accounts.zoho.eu", "https://vault.zoho.eu")
        );
        let ep = Endpoints::of("ca").unwrap();
        assert_eq!(ep.vault, "https://vault.zohocloud.ca");
        for dc in [
            "",
            "evil.com",
            "https://evil.com",
            "eu.evil.com",
            "127.0.0.1",
        ] {
            assert!(Endpoints::of(dc).is_none(), "{dc}");
        }
    }

    // ---- against a mock of Zoho's API -----------------------------------------

    use axum::extract::{Path as AxPath, Query, State};
    use axum::http::HeaderMap;
    use axum::routing::{get as aget, post};
    use axum::{Form, Json, Router};
    use std::sync::Arc;

    #[derive(Default)]
    struct Mock {
        token_requests: Mutex<u32>,
        bad_auth: Mutex<u32>,
        /// Answer the first API call with INVALID_OAUTHTOKEN.
        expire_once: Mutex<bool>,
        /// Secrets named `aaa-filler-NNNN` listed before the others.
        filler: Mutex<usize>,
        /// `pageNum` counts from 1 (0 is the first page too).
        one_based: Mutex<bool>,
        /// Calls to the secrets list.
        list_calls: Mutex<u32>,
    }

    /// Zoho's request filter: an argument the URL doesn't declare is
    /// refused (EXTRA_PARAM_FOUND, the code repeated as the message), and so
    /// is a missing mandatory one.
    fn refused(
        q: &HashMap<String, String>,
        allowed: &[&str],
        required: &[&str],
    ) -> Option<Json<Value>> {
        if q.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Some(failed("EXTRA_PARAM_FOUND"));
        }
        required
            .iter()
            .any(|r| !q.contains_key(*r))
            .then(|| failed("LESS_THAN_MIN_OCCURANCE"))
    }

    fn authorized(h: &HeaderMap, m: &Mock) -> bool {
        let ok =
            h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Zoho-oauthtoken at-1");
        if !ok {
            *m.bad_auth.lock().unwrap() += 1;
        }
        ok
    }

    fn failed(code: &str) -> Json<Value> {
        Json(
            json!({ "operation": { "result": { "status": "failed", "error_code": code, "message": code } } }),
        )
    }

    async fn mock_server() -> (String, Arc<Mock>) {
        let state = Arc::new(Mock::default());
        let v = vectors();
        let v2 = v.clone();
        let v3 = v.clone();
        let app = Router::new()
            .route(
                "/oauth/v2/token",
                post(|State(m): State<Arc<Mock>>, Form(f): Form<HashMap<String, String>>| async move {
                    *m.token_requests.lock().unwrap() += 1;
                    if f.get("grant_type").map(String::as_str) != Some("refresh_token") || f.get("client_id").map(String::as_str) != Some("1000.CLIENT") {
                        return Json(json!({ "error": "invalid_client" }));
                    }
                    if f.get("client_secret").map(String::as_str) != Some("dummy-client-secret") {
                        return Json(json!({ "error": "invalid_client" }));
                    }
                    if f.get("refresh_token").map(String::as_str) != Some("1000.dummy-refresh") {
                        return Json(json!({ "error": "invalid_code" }));
                    }
                    Json(json!({ "access_token": "at-1", "expires_in": 3600, "api_domain": "https://www.zohoapis.eu", "token_type": "Bearer" }))
                }),
            )
            .route(
                "/api/json/login",
                aget(move |State(m): State<Arc<Mock>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>| {
                    let v = v.clone();
                    async move {
                        if std::mem::take(&mut *m.expire_once.lock().unwrap()) {
                            return failed("INVALID_OAUTHTOKEN");
                        }
                        if let Some(e) = refused(&q, &["OPERATION_NAME"], &["OPERATION_NAME"]) {
                            return e;
                        }
                        if !authorized(&h, &m) || q.get("OPERATION_NAME").map(String::as_str) != Some("GET_LOGIN") {
                            return failed("INVALID_TICKET");
                        }
                        Json(json!({ "operation": { "result": { "status": "success", "message": "" }, "name": "GET_LOGIN", "details": {
                            "PASSPHRASE": v["passphrase_check"], "SALT": v["salt"], "ITERATION": v["iterations"], "LOGIN": "PBKDF2_AES"
                        } } }))
                    }
                }),
            )
            .route(
                "/api/rest/json/v1/secrets",
                aget(move |State(m): State<Arc<Mock>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>| {
                    let v = v2.clone();
                    async move {
                        if !authorized(&h, &m) {
                            return failed("INVALID_OAUTHTOKEN");
                        }
                        // The documented arguments but `passwordName`, which
                        // the real Zoho refuses with EXTRA_PARAM_FOUND.
                        let allowed = ["isAsc", "pageNum", "rowPerPage", "filter", "passwordType", "chamberId"];
                        if let Some(e) = refused(&q, &allowed, &["isAsc", "pageNum", "rowPerPage"]) {
                            return e;
                        }
                        *m.list_calls.lock().unwrap() += 1;
                        let mut shared = secret("org-db", "2000000000002", true, s(&v, "shared_secret_data"));
                        shared["customcolumn"] = json!(s(&v, "shared_custom_column"));
                        let mut all: Vec<Value> = (0..*m.filler.lock().unwrap()).map(|i| secret(&format!("aaa-filler-{i:04}"), &format!("{}", 3000000000000u64 + i as u64), false, "{}")).collect();
                        all.extend([
                            secret("pg-main", "2000000000001", false, s(&v, "personal_secret_data")),
                            shared,
                            secret("twin", "2000000000003", false, "{}"),
                            secret("twin", "2000000000004", false, "{}"),
                            secret("AZURE - BRAZIL- PROD - SQL - ADMIN", "2000000000005", false, s(&v, "personal_secret_data")),
                        ]);
                        all.sort_by_key(|s| name_of(s).to_lowercase());
                        let rows: usize = q["rowPerPage"].parse().unwrap();
                        let mut page: usize = q["pageNum"].parse().unwrap();
                        if *m.one_based.lock().unwrap() {
                            page = page.saturating_sub(1);
                        }
                        let hits: Vec<Value> = all.into_iter().skip(page * rows).take(rows).collect();
                        Json(json!({ "operation": { "result": { "status": "Success", "message": "" }, "name": "GET_SECRETS", "Details": hits } }))
                    }
                }),
            )
            .route(
                "/api/rest/json/v1/secrets/{id}",
                aget(move |State(m): State<Arc<Mock>>, h: HeaderMap, AxPath(id): AxPath<String>, Query(q): Query<HashMap<String, String>>| {
                    let v = v3.clone();
                    async move {
                        if let Some(e) = refused(&q, &[], &[]) {
                            return e;
                        }
                        if !authorized(&h, &m) {
                            return failed("INVALID_OAUTHTOKEN");
                        }
                        if id != "2000000000001" {
                            return failed("SECRET_NOT_FOUND");
                        }
                        Json(json!({ "operation": { "result": { "status": "Success", "message": "" }, "name": "GET_SECRET",
                            "Details": [secret("pg-main", "2000000000001", false, s(&v, "personal_secret_data"))] } }))
                    }
                }),
            )
            .route(
                "/api/rest/json/v1/user/orgkey",
                aget(|State(m): State<Arc<Mock>>, h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                    if let Some(e) = refused(&q, &[], &[]) {
                        return e;
                    }
                    if !authorized(&h, &m) {
                        return failed("INVALID_OAUTHTOKEN");
                    }
                    let v = vectors();
                    Json(json!({ "operation": { "result": { "status": "Success", "message": "" }, "Details": {
                        "privateKey": v["rsa1024_private_key"], "sharedKey": v["rsa1024_shared_key"]
                    } } }))
                }),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), state)
    }

    fn creds(passphrase: &str, refresh: &str) -> Creds {
        Creds {
            client_id: "1000.CLIENT".into(),
            client_secret: "dummy-client-secret".into(),
            refresh_token: refresh.into(),
            passphrase: passphrase.into(),
        }
    }

    /// Everything against a mock of Zoho's endpoints (no Zoho account was
    /// available): token refresh, GET_LOGIN, by name and by ID, personal and
    /// shared secrets, the organization key, error mapping, token reuse.
    #[tokio::test]
    async fn against_a_mock_of_the_api() {
        let (base, mock) = mock_server().await;
        let ep = Endpoints {
            accounts: base.clone(),
            vault: base.clone(),
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let v = vectors();
        let pass = s(&v, "passphrase").to_string();
        let r = |secret: &'static str, attribute: &'static str, username: bool| Request {
            data_center: "eu",
            secret,
            attribute,
            username,
            user_field: "",
        };
        // Each test uses its own refresh token: the access token cache is
        // keyed by it.
        let c = creds(&pass, "1000.dummy-refresh");
        forget_token(&token_cache_key(&ep, &c));
        assert_eq!(
            read_with(&client, &ep, &r("pg-main", "", true), &c)
                .await
                .unwrap(),
            ("pg-secret-1".into(), Some("app_user".into()))
        );
        assert_eq!(
            read_with(&client, &ep, &r("2000000000001", "username", false), &c)
                .await
                .unwrap()
                .0,
            "app_user",
            "by ID"
        );
        assert_eq!(
            read_with(&client, &ep, &r("org-db", "", true), &c)
                .await
                .unwrap(),
            ("org-secret-9".into(), Some("org_user".into())),
            "shared: the organization key"
        );
        assert_eq!(
            read_with(&client, &ep, &r("org-db", "Port", false), &c)
                .await
                .unwrap()
                .0,
            "5432"
        );
        assert_eq!(
            *mock.token_requests.lock().unwrap(),
            1,
            "one access token for all of them"
        );
        // Zoho says the access token expired: a new one, once.
        *mock.expire_once.lock().unwrap() = true;
        assert_eq!(
            read_with(&client, &ep, &r("pg-main", "", false), &c)
                .await
                .unwrap()
                .0,
            "pg-secret-1"
        );
        assert_eq!(*mock.token_requests.lock().unwrap(), 2);
        let e = read_with(&client, &ep, &r("twin", "", false), &c)
            .await
            .unwrap_err();
        assert!(
            e.contains("2 secretos «twin»") && e.contains("2000000000003"),
            "{e}"
        );
        let e = read_with(&client, &ep, &r("nothing", "", false), &c)
            .await
            .unwrap_err();
        assert!(e.contains("no hay un secreto «nothing»"), "{e}");
        let e = read_with(&client, &ep, &r("2000000000999", "", false), &c)
            .await
            .unwrap_err();
        assert!(e.contains("SECRET_NOT_FOUND"), "{e}");
        let e = read_with(
            &client,
            &ep,
            &r("pg-main", "", false),
            &creds("wrong passphrase", "1000.dummy-refresh"),
        )
        .await
        .unwrap_err();
        assert_eq!(e, WRONG_PASSPHRASE);
        // A wrong refresh token or client secret: Zoho's answer, never the values.
        let bad = creds(&pass, "1000.revoked");
        let e = read_with(&client, &ep, &r("pg-main", "", false), &bad)
            .await
            .unwrap_err();
        assert!(
            e.contains("token de actualización") && !e.contains("revoked"),
            "{e}"
        );
        let mut bad = creds(&pass, "1000.dummy-refresh-2");
        bad.client_secret = "nope".into();
        let e = read_with(&client, &ep, &r("pg-main", "", false), &bad)
            .await
            .unwrap_err();
        assert!(
            e.contains("secreto del cliente") && !e.contains("nope"),
            "{e}"
        );
        assert_eq!(
            *mock.bad_auth.lock().unwrap(),
            0,
            "every API call carried the access token"
        );
        forget_token(&token_cache_key(&ep, &c));
    }

    /// Browsing: names (a repeated one by its ID), then a secret's field
    /// names, never a value or its ciphertext.
    #[tokio::test]
    async fn browse_against_the_mock() {
        let (base, _) = mock_server().await;
        let ep = Endpoints {
            accounts: base.clone(),
            vault: base.clone(),
        };
        let client = reqwest::Client::new();
        let c = creds("", "1000.dummy-refresh");
        let l = browse_with(&client, &ep, &c, Default::default())
            .await
            .unwrap();
        let ids: Vec<&str> = l.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "AZURE - BRAZIL- PROD - SQL - ADMIN",
                "org-db",
                "pg-main",
                "2000000000003",
                "2000000000004"
            ]
        );
        let at = crate::secret_sources::browse::At {
            item: Some("pg-main"),
            ..Default::default()
        };
        let l = browse_with(&client, &ep, &c, at).await.unwrap();
        let fields = l.fields.clone().unwrap();
        assert!(
            fields.contains(&"password".to_string()) && fields.contains(&"username".to_string()),
            "{fields:?}"
        );
        let v = vectors();
        let json = serde_json::to_string(&l).unwrap();
        assert!(!json.contains(s(&v, "personal_secret_data")), "{json}");
        forget_token(&token_cache_key(&ep, &c));
    }

    /// The browse's search text is the UI's filter, never a Zoho argument;
    /// picking a name with spaces and hyphens lists its fields (the
    /// owner's EXTRA_PARAM_FOUND), and connecting by it reads the value.
    #[tokio::test]
    async fn a_searched_name_with_spaces_and_hyphens() {
        let (base, _) = mock_server().await;
        let ep = Endpoints {
            accounts: base.clone(),
            vault: base.clone(),
        };
        let client = reqwest::Client::new();
        let name = "AZURE - BRAZIL- PROD - SQL - ADMIN";
        let c = creds("", "1000.dummy-refresh");
        let at = crate::secret_sources::browse::At {
            query: "AZURE - BRAZIL- PROD",
            ..Default::default()
        };
        let l = browse_with(&client, &ep, &c, at).await.unwrap();
        assert!(
            l.complete && l.entries.iter().any(|e| e.id == name),
            "{:?}",
            l.entries
        );
        let at = crate::secret_sources::browse::At {
            item: Some(name),
            query: "AZURE - BRAZIL- PROD",
            ..Default::default()
        };
        let fields = browse_with(&client, &ep, &c, at)
            .await
            .unwrap()
            .fields
            .unwrap();
        assert!(fields.contains(&"password".to_string()), "{fields:?}");
        forget_token(&token_cache_key(&ep, &c));
        let v = vectors();
        let c = creds(s(&v, "passphrase"), "1000.dummy-refresh");
        let r = |secret: &'static str| Request {
            data_center: "eu",
            secret,
            attribute: "",
            username: true,
            user_field: "",
        };
        assert_eq!(
            read_with(&client, &ep, &r(name), &c).await.unwrap(),
            ("pg-secret-1".into(), Some("app_user".into()))
        );
        assert_eq!(
            read_with(&client, &ep, &r("azure - brazil- prod - sql - admin"), &c)
                .await
                .unwrap()
                .0,
            "pg-secret-1",
            "ignoring case"
        );
        let e = read_with(&client, &ep, &r("AZURE - BRAZIL"), &c)
            .await
            .unwrap_err();
        assert!(
            e.contains("no hay un secreto «AZURE - BRAZIL»") && e.contains(name),
            "similar names: {e}"
        );
        forget_token(&token_cache_key(&ep, &c));
    }

    /// By name across pages: the list is read with the paging arguments
    /// only, until a short page; twins on different pages are still seen.
    #[tokio::test]
    async fn by_name_across_pages() {
        let (base, mock) = mock_server().await;
        *mock.filler.lock().unwrap() = 250;
        let ep = Endpoints {
            accounts: base.clone(),
            vault: base.clone(),
        };
        let client = reqwest::Client::new();
        let v = vectors();
        let c = creds(s(&v, "passphrase"), "1000.dummy-refresh");
        let r = |secret: &'static str| Request {
            data_center: "eu",
            secret,
            attribute: "",
            username: false,
            user_field: "",
        };
        assert_eq!(
            read_with(&client, &ep, &r("pg-main"), &c).await.unwrap().0,
            "pg-secret-1"
        );
        assert_eq!(
            *mock.list_calls.lock().unwrap(),
            3,
            "255 secrets: pages 0, 1 and 2"
        );
        assert_eq!(
            read_with(&client, &ep, &r("org-db"), &c).await.unwrap().0,
            "org-secret-9",
            "shared, on the last page"
        );
        let e = read_with(&client, &ep, &r("twin"), &c).await.unwrap_err();
        assert!(e.contains("2 secretos «twin»"), "{e}");
        let l = browse_with(&client, &ep, &c, Default::default())
            .await
            .unwrap();
        assert_eq!(l.entries.len(), 255);
        assert!(!l.truncated);
        // If pageNum counted from 1, page 0 would repeat page 1: nothing twice.
        *mock.one_based.lock().unwrap() = true;
        let l = browse_with(&client, &ep, &c, Default::default())
            .await
            .unwrap();
        assert_eq!(l.entries.len(), 255, "no repeated page");
        assert_eq!(
            read_with(&client, &ep, &r("pg-main"), &c).await.unwrap().0,
            "pg-secret-1"
        );
        forget_token(&token_cache_key(&ep, &c));
    }

    /// The mock refuses what Zoho refuses, and the message says it once.
    #[tokio::test]
    async fn an_unknown_argument_is_refused() {
        let (base, _) = mock_server().await;
        let ep = Endpoints {
            accounts: base.clone(),
            vault: base.clone(),
        };
        let client = reqwest::Client::new();
        let c = creds("", "1000.dummy-refresh");
        let e = api(
            &client,
            &ep,
            &c,
            "/api/rest/json/v1/secrets?isAsc=true&pageNum=0&rowPerPage=100&passwordName=x",
        )
        .await
        .unwrap_err();
        assert!(
            e.starts_with("Zoho Vault rechazó el pedido (EXTRA_PARAM_FOUND)")
                && e.matches("EXTRA_PARAM_FOUND").count() == 1,
            "{e}"
        );
        assert!(api(&client, &ep, &c, "/api/rest/json/v1/user/orgkey?x=1")
            .await
            .unwrap_err()
            .contains("EXTRA_PARAM_FOUND"));
        assert!(api(&client, &ep, &c, "/api/rest/json/v1/secrets?pageNum=0")
            .await
            .unwrap_err()
            .contains("LESS_THAN_MIN_OCCURANCE"));
        forget_token(&token_cache_key(&ep, &c));
    }

    // ---- connecting an account ------------------------------------------------

    /// Zoho's accounts servers, one per data centre, under `/{dc}`: the Self
    /// Client lives in `eu`; the others don't know it (`invalid_client`),
    /// as Zoho answers. `eu` also checks the secret and the code (`used`
    /// once, `expired` never valid).
    async fn dc_mock() -> (String, Arc<Mutex<Vec<String>>>) {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let app = Router::new()
            .route(
                "/{dc}/oauth/v2/token",
                post(|State(seen): State<Arc<Mutex<Vec<String>>>>, AxPath(dc): AxPath<String>, Form(f): Form<HashMap<String, String>>| async move {
                    seen.lock().unwrap().push(dc.clone());
                    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
                    if dc != "eu" || get("client_id") != "1000.CLIENT" || get("grant_type") != "authorization_code" {
                        return Json(json!({ "error": "invalid_client" }));
                    }
                    if get("client_secret") != "dummy-client-secret" {
                        return Json(json!({ "error": "invalid_client_secret" }));
                    }
                    if get("code") != "1000.good-code" {
                        return Json(json!({ "error": "invalid_code" }));
                    }
                    Json(json!({ "access_token": "at-new", "refresh_token": "1000.rt-new", "api_domain": "https://www.zohoapis.eu", "token_type": "Bearer", "expires_in": 3600 }))
                }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn servers(base: &str) -> Vec<(String, String)> {
        ["com", "eu", "in", "jp"]
            .iter()
            .map(|dc| (dc.to_string(), format!("{base}/{dc}")))
            .collect()
    }

    /// The data centre is found by trying the code: `invalid_client`
    /// elsewhere, a token at the Self Client's own. It stops there.
    #[tokio::test]
    async fn the_data_centre_is_detected() {
        let (base, seen) = dc_mock().await;
        let client = reqwest::Client::new();
        let ex = exchange_code_with(
            &client,
            &servers(&base),
            " 1000.CLIENT ",
            "dummy-client-secret",
            "1000.good-code\n",
        )
        .await
        .unwrap();
        assert_eq!(
            (
                ex.data_center.as_str(),
                ex.refresh_token.as_str(),
                ex.access_token.as_str()
            ),
            ("eu", "1000.rt-new", "at-new")
        );
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["com", "eu"],
            "tried in order, stopped at the one"
        );
    }

    /// Expired or used codes, a wrong secret, an unknown client: each its
    /// own message, none with the values typed.
    #[tokio::test]
    async fn exchange_errors() {
        let (base, _) = dc_mock().await;
        let client = reqwest::Client::new();
        let e = exchange_code_with(
            &client,
            &servers(&base),
            "1000.CLIENT",
            "dummy-client-secret",
            "1000.old-code",
        )
        .await
        .err()
        .unwrap();
        assert_eq!(e, CODE_EXPIRED);
        assert!(e.contains("generá un código nuevo"));
        let e = exchange_code_with(
            &client,
            &servers(&base),
            "1000.CLIENT",
            "nope-secret",
            "1000.good-code",
        )
        .await
        .err()
        .unwrap();
        assert_eq!(e, WRONG_CLIENT_SECRET);
        let e = exchange_code_with(
            &client,
            &servers(&base),
            "1000.OTHER",
            "dummy-client-secret",
            "1000.good-code",
        )
        .await
        .err()
        .unwrap();
        assert_eq!(e, UNKNOWN_CLIENT);
        assert!(!e.contains("1000.OTHER") && !e.contains("dummy-client-secret"));
        assert!(exchange_code_with(
            &client,
            &servers(&base),
            "1000.CLIENT",
            "dummy-client-secret",
            "  "
        )
        .await
        .is_err());
        // A server that can't be reached is skipped, and said when nothing answered.
        let down = vec![("com".to_string(), "http://127.0.0.1:9".to_string())];
        let e = exchange_code_with(&client, &down, "1000.CLIENT", "s", "c")
            .await
            .err()
            .unwrap();
        assert!(e.contains("no se pudo consultar a Zoho en com"), "{e}");
    }

    /// The code and the secret only go to Zoho's accounts servers, over
    /// HTTPS: the data centres' table and the ones Zoho lists without a
    /// known Vault host. The production client refuses plain HTTP and
    /// doesn't follow redirects.
    #[tokio::test]
    async fn only_zohos_accounts_servers() {
        let list = accounts_servers();
        assert!(list.len() >= 12);
        for (dc, url) in &list {
            let host = url
                .strip_prefix("https://")
                .unwrap_or_else(|| panic!("{dc}: {url}"));
            assert!(host.starts_with("accounts."), "{dc}: {host}");
            let domain = host.trim_start_matches("accounts.");
            assert!(
                ["zoho.", "zohocloud.ca", "zohohq.in"]
                    .iter()
                    .any(|d| domain.starts_with(d)),
                "{dc}: {domain}"
            );
            assert!(!host.contains(['/', '@', ':', '?']), "{dc}: {host}");
        }
        assert!(
            list.iter()
                .any(|(dc, u)| dc == "inec" && u == "https://accounts.zohohq.in"),
            "Zoho's live list has it"
        );
        let (base, seen) = dc_mock().await;
        let e = exchange_code_with(
            &https_client().unwrap(),
            &servers(&base),
            "1000.CLIENT",
            "dummy-client-secret",
            "1000.good-code",
        )
        .await
        .err()
        .unwrap();
        assert!(e.contains("no se pudo consultar"), "{e}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing went over plain HTTP"
        );
    }

    /// GET_LOGIN with the new token: the user for the label, and only what
    /// checks the passphrase is kept.
    #[tokio::test]
    async fn the_user_and_the_passphrase_check() {
        let (base, _) = mock_server().await;
        let client = reqwest::Client::new();
        let d = login_details_with(&client, &base, "at-1").await.unwrap();
        let v = vectors();
        assert_eq!(identity_of(&d), "", "the mock has no USER");
        assert_eq!(
            identity_of(&json!({ "USER": { "USERNAME": "ana", "EMAIL": "ana@example.com" } })),
            "ana@example.com"
        );
        assert_eq!(
            identity_of(&json!({ "USER": { "USERNAME": "ana", "EMAIL": "" } })),
            "ana"
        );
        let check = passphrase_check(&d);
        assert_eq!(
            master_key(&check, s(&v, "passphrase")).unwrap(),
            s(&v, "master")
        );
        assert_eq!(master_key(&check, "wrong").unwrap_err(), WRONG_PASSPHRASE);
        let e = login_details_with(&client, &base, "bad-token")
            .await
            .unwrap_err();
        assert!(e.contains(SCOPES), "{e}");
    }

    /// Two connections through one account: the same credentials, so one
    /// access token serves both (Zoho allows 10 refreshes per 10 minutes);
    /// forgetting the account's token makes the next use refresh.
    #[tokio::test]
    async fn one_access_token_per_account() {
        let (base, mock) = mock_server().await;
        let ep = Endpoints {
            accounts: base.clone(),
            vault: base.clone(),
        };
        let client = reqwest::Client::new();
        let v = vectors();
        let account = || creds(s(&v, "passphrase"), "1000.dummy-refresh-acct");
        forget_token(&token_cache_key(&ep, &account()));
        let conn_a = Request {
            data_center: "eu",
            secret: "pg-main",
            attribute: "",
            username: false,
            user_field: "",
        };
        let conn_b = Request {
            data_center: "eu",
            secret: "org-db",
            attribute: "",
            username: false,
            user_field: "",
        };
        let before = *mock.token_requests.lock().unwrap();
        // The mock only knows `1000.dummy-refresh`: seed as the wizard does.
        seed_token(&ep, &account(), "at-1", 3600);
        assert_eq!(
            read_with(&client, &ep, &conn_a, &account())
                .await
                .unwrap()
                .0,
            "pg-secret-1"
        );
        assert_eq!(
            read_with(&client, &ep, &conn_b, &account())
                .await
                .unwrap()
                .0,
            "org-secret-9"
        );
        assert_eq!(
            *mock.token_requests.lock().unwrap(),
            before,
            "the token the exchange gave serves both connections"
        );
        forget_token(&token_cache_key(&ep, &account()));
        let c = creds(s(&v, "passphrase"), "1000.dummy-refresh");
        forget_token(&token_cache_key(&ep, &c));
        read_with(&client, &ep, &conn_a, &c).await.unwrap();
        read_with(&client, &ep, &conn_b, &c).await.unwrap();
        assert_eq!(
            *mock.token_requests.lock().unwrap(),
            before + 1,
            "one refresh for both"
        );
        forget_token(&token_cache_key(&ep, &c));
    }

    /// The real endpoints refuse plain HTTP: `https_only`.
    #[tokio::test]
    async fn production_client_is_https_only() {
        let (base, _) = mock_server().await;
        let client = reqwest::Client::builder().https_only(true).build().unwrap();
        let ep = Endpoints {
            accounts: base.clone(),
            vault: base,
        };
        let c = creds("x", "1000.dummy-refresh-https");
        let e = read_with(
            &client,
            &ep,
            &Request {
                data_center: "eu",
                secret: "pg-main",
                attribute: "",
                username: false,
                user_field: "",
            },
            &c,
        )
        .await
        .unwrap_err();
        assert!(e.starts_with("no se pudo conectar con Zoho"), "{e}");
    }
}

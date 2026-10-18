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
//! 4. The secret, by ID or by name. A personal secret is encrypted with the
//!    master key; a shared one (`isshared: YES`) with the organization key,
//!    which is RSA-encrypted for the user with a private key that is itself
//!    encrypted with the master key (`/user/orgkey`).
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
        Some(Self { accounts: format!("https://{accounts}"), vault: format!("https://{vault}") })
    }
}

/// `(value, username)`.
pub async fn read(req: &Request<'_>, creds: &Creds) -> Result<(String, Option<String>), String> {
    let ep = Endpoints::of(req.data_center).ok_or_else(|| format!("el centro de datos de Zoho «{}» no es válido", req.data_center.trim()))?;
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
    for part in [ep.accounts.as_str(), creds.client_id.as_str(), creds.refresh_token.as_str()] {
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

async fn access_token(client: &reqwest::Client, ep: &Endpoints, creds: &Creds) -> Result<String, String> {
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
        .map_err(|e| format!("no se pudo conectar con Zoho ({}): {}", ep.accounts, e.without_url()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(t) = json.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        let secs = json.get("expires_in").and_then(Value::as_u64).unwrap_or(3600).clamp(120, 3600);
        TOKENS.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now() + Duration::from_secs(secs - 60), t.to_string()));
        return Ok(t.to_string());
    }
    let code = json.get("error").and_then(Value::as_str).unwrap_or("");
    let desc = json.get("error_description").and_then(Value::as_str).unwrap_or("");
    Err(oauth_error(status, code, desc))
}

pub fn oauth_error(status: u16, code: &str, desc: &str) -> String {
    match code {
        "invalid_client" | "invalid_client_secret" => "Zoho rechazó el ID o el secreto del cliente (¿son de este centro de datos?)".into(),
        "invalid_code" | "invalid_grant" => "el token de actualización de Zoho no es válido, venció o se revocó: generá uno nuevo".into(),
        "Access Denied" | "access_denied" => format!("Zoho no permite pedir tokens tan seguido; probá en unos minutos ({})", super::exec::first_lines(desc, 200)),
        "" if status == 200 => "Zoho no devolvió un token de acceso".into(),
        "" => format!("Zoho respondió {status} al pedir el token de acceso"),
        _ => format!("Zoho rechazó el pedido del token de acceso: {}", super::exec::first_lines(code, 100)),
    }
}

// ---- API ---------------------------------------------------------------------

/// GET a Vault API path; `Err(None)` when Zoho says the access token is no
/// longer valid (the caller asks for a new one once).
async fn get(client: &reqwest::Client, ep: &Endpoints, token: &str, path_and_query: &str) -> Result<Value, Option<String>> {
    let resp = client
        .get(format!("{}{path_and_query}", ep.vault))
        .header("Authorization", format!("Zoho-oauthtoken {token}"))
        .send()
        .await
        .map_err(|e| Some(format!("no se pudo conectar con Zoho Vault ({}): {}", ep.vault, e.without_url())))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let result = json.pointer("/operation/result");
    let failed = result.and_then(|r| r.get("status")).and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("failed"));
    if failed || !(200..300).contains(&status) {
        let code = result.and_then(|r| r.get("error_code")).and_then(Value::as_str).unwrap_or("");
        let msg = result.and_then(|r| r.get("message")).and_then(Value::as_str).unwrap_or("");
        if matches!(code, "INVALID_OAUTHTOKEN" | "INVALID_TICKET") || status == 401 {
            return Err(None);
        }
        return Err(Some(api_error(status, code, msg)));
    }
    Ok(json)
}

pub fn api_error(status: u16, code: &str, msg: &str) -> String {
    let msg = super::exec::first_lines(msg, 300);
    match code {
        "INVALID_OAUTHSCOPE" | "OAUTH_SCOPE_MISMATCH" => "el token de Zoho no tiene los permisos necesarios: generalo con ZohoVault.secrets.READ y ZohoVault.user.READ".into(),
        "NO_PERMISSION" | "PERMISSION_DENIED" => format!("Zoho Vault: permiso denegado: {msg}"),
        _ if !code.is_empty() => format!("Zoho Vault respondió {code}: {msg}"),
        _ if msg.is_empty() => format!("Zoho Vault respondió {status}"),
        _ => format!("Zoho Vault respondió {status}: {msg}"),
    }
}

/// GET with the access token, asking for a new one once if Zoho says it's
/// no longer valid.
async fn api(client: &reqwest::Client, ep: &Endpoints, creds: &Creds, path_and_query: &str) -> Result<Value, String> {
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

fn uri_encode(s: &str) -> String {
    super::cloud_auth::uri_encode(s)
}

/// `(value, username)` of the secret, talking to `ep`.
pub async fn read_with(client: &reqwest::Client, ep: &Endpoints, req: &Request<'_>, creds: &Creds) -> Result<(String, Option<String>), String> {
    let login = api(client, ep, creds, "/api/json/login?OPERATION_NAME=GET_LOGIN").await?;
    let details = login.pointer("/operation/details").ok_or("Zoho Vault no devolvió los datos de la cuenta (GET_LOGIN)")?;
    let mut master = master_key(details, &creds.passphrase)?;
    let secret = find_secret(client, ep, creds, req.secret).await;
    let out = match secret {
        Ok(secret) => {
            let shared = secret.get("isshared").and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("YES"));
            let key = if shared { org_key(client, ep, creds, &master).await } else { Ok(master.clone()) };
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
    let check = details.get("PASSPHRASE").and_then(Value::as_str).unwrap_or("");
    let kind = details.get("LOGIN").and_then(Value::as_str).unwrap_or("PBKDF2_AES");
    if kind.eq_ignore_ascii_case("SHA256") {
        let key = super::cloud_auth::hex(&Sha256::digest(passphrase.as_bytes()));
        // Older accounts: the check is SHA-256 of the passphrase and salt.
        if check.len() == 64 && check.chars().all(|c| c.is_ascii_hexdigit()) {
            let auth = super::cloud_auth::hex(&Sha256::digest(format!("{passphrase}{salt}").as_bytes()));
            if !auth.eq_ignore_ascii_case(check) {
                return Err(WRONG_PASSPHRASE.into());
            }
        }
        return Ok(key);
    }
    if salt.is_empty() {
        return Err("Zoho Vault no devolvió la sal de la cuenta (GET_LOGIN)".into());
    }
    let iterations = details.get("ITERATION").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))).unwrap_or(1000);
    if !(1..=10_000_000).contains(&iterations) {
        return Err(format!("Zoho Vault devolvió una cantidad de iteraciones fuera de rango ({iterations})"));
    }
    let key = pbkdf2_key(passphrase, salt, iterations as u32);
    // The check value decrypts to a small JSON object with this key only.
    if !check.is_empty() {
        let ok = decrypt(check, &key).ok().and_then(|b| String::from_utf8(b).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()).is_some_and(|v| v.is_object());
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

/// The secret's JSON: by ID (digits), else by exact name.
async fn find_secret(client: &reqwest::Client, ep: &Endpoints, creds: &Creds, wanted: &str) -> Result<Value, String> {
    let wanted = wanted.trim();
    if !wanted.is_empty() && wanted.chars().all(|c| c.is_ascii_digit()) {
        let json = api(client, ep, creds, &format!("/api/rest/json/v1/secrets/{wanted}")).await?;
        return first_detail(&json).ok_or_else(|| format!("Zoho Vault: no existe el secreto con el ID {wanted}"));
    }
    let q = format!("/api/rest/json/v1/secrets?isAsc=true&pageNum=0&rowPerPage=100&passwordName={}", uri_encode(wanted));
    let json = api(client, ep, creds, &q).await?;
    let list: Vec<Value> = json.pointer("/operation/Details").and_then(Value::as_array).cloned().unwrap_or_default();
    pick_by_name(list, wanted)
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
    let found = if exact.is_empty() { list.iter().filter(|s| name_of(s).eq_ignore_ascii_case(wanted)).collect() } else { exact };
    match found.len() {
        1 => Ok(found[0].clone()),
        0 => {
            let mut names: Vec<&str> = list.iter().map(name_of).filter(|n| !n.is_empty()).take(8).collect();
            names.sort_unstable();
            names.dedup();
            if names.is_empty() {
                Err(format!("Zoho Vault: no hay un secreto «{wanted}» (o el usuario no tiene acceso)"))
            } else {
                Err(format!("Zoho Vault: no hay un secreto «{wanted}» (parecidos: {})", names.join(", ")))
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
async fn org_key(client: &reqwest::Client, ep: &Endpoints, creds: &Creds, master: &str) -> Result<String, String> {
    let json = api(client, ep, creds, "/api/rest/json/v1/user/orgkey").await?;
    let d = first_detail(&json).ok_or("Zoho Vault no devolvió la clave de la organización")?;
    let private = d.get("privateKey").and_then(Value::as_str).filter(|s| !s.is_empty() && *s != "null");
    let shared = d.get("sharedKey").and_then(Value::as_str).filter(|s| !s.is_empty() && *s != "null");
    match (private, shared) {
        (Some(p), Some(s)) => decrypt_org_key(p, s, master),
        _ => Err("el secreto es compartido, pero Zoho Vault no devolvió la clave de la organización para este usuario".into()),
    }
}

pub fn decrypt_org_key(private_key: &str, shared_key: &str, master: &str) -> Result<String, String> {
    use rsa::pkcs8::DecodePrivateKey;
    let bad_key = || "no se pudo descifrar la clave de la organización de Zoho Vault".to_string();
    let mut b64 = decrypt(private_key, master).ok().and_then(|b| String::from_utf8(b).ok()).ok_or_else(bad_key)?;
    let der = base64::engine::general_purpose::STANDARD.decode(b64.trim().as_bytes());
    wipe(&mut b64);
    let mut der = der.map_err(|_| bad_key())?;
    let key = rsa::RsaPrivateKey::from_pkcs8_der(&der);
    wipe_bytes(&mut der);
    let key = key.map_err(|_| bad_key())?;
    let ct = base64::engine::general_purpose::STANDARD.decode(shared_key.trim().as_bytes()).map_err(|_| bad_key())?;
    // OAEP with SHA-256 (what current clients write), else PKCS#1 v1.5.
    let plain = key.decrypt(rsa::Oaep::<rsa::sha2::Sha256>::new(), &ct).or_else(|_| key.decrypt(rsa::Pkcs1v15Encrypt, &ct)).map_err(|_| bad_key())?;
    String::from_utf8(plain).map_err(|_| bad_key())
}

/// The secret's data: `secretData` (a JSON object, or its text) plus the
/// custom fields (`customcolumn`: base64 of `{"customcol":[{colname,value}]}`).
fn fields_of(secret: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let data = secret.get("secretData").or_else(|| secret.get("SECRETDATA"));
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
    let custom = secret.get("customcolumn").or_else(|| secret.get("CUSTOMCOLUMN")).and_then(Value::as_str).filter(|s| !s.is_empty());
    if let Some(c) = custom {
        let parsed = base64::engine::general_purpose::STANDARD
            .decode(c.trim().as_bytes())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        if let Some(cols) = parsed.as_ref().and_then(|p| p.get("customcol")).and_then(Value::as_array) {
            for col in cols {
                if let (Some(n), Some(v)) = (col.get("colname").and_then(Value::as_str), col.get("value").and_then(Value::as_str)) {
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

pub fn values(secret: &Value, key: &str, req: &Request<'_>) -> Result<(String, Option<String>), String> {
    let fields = fields_of(secret);
    let name = Some(name_of(secret)).filter(|n| !n.is_empty()).unwrap_or(req.secret.trim()).to_string();
    let want = Some(req.attribute.trim()).filter(|a| !a.is_empty()).unwrap_or("password");
    let find = |w: &str| fields.iter().find(|(k, _)| k == w).or_else(|| fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(w)));
    let Some((k, c)) = find(want) else {
        let mut names: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).take(30).collect();
        names.sort_unstable();
        return Err(format!("el secreto «{name}» de Zoho Vault no tiene el campo «{want}» (tiene: {})", names.join(", ")));
    };
    let value = open_value(c, key, k, &name)?;
    let username = if req.username {
        match find("username") {
            Some((k, c)) => Some(open_value(c, key, k, &name)?).filter(|u| !u.is_empty()),
            None => None,
        }
    } else {
        None
    };
    if req.username && username.is_none() {
        return Err(format!("el secreto «{name}» de Zoho Vault no tiene usuario (username)"));
    }
    Ok((value, username))
}

// ---- browse ------------------------------------------------------------------

/// The secrets' names (paged), or a secret's field names (`item`, an ID or
/// a name). Neither is encrypted: no passphrase, nothing decrypted.
pub async fn browse(data_center: &str, creds: &Creds, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    let ep = Endpoints::of(data_center).ok_or_else(|| format!("el centro de datos de Zoho «{}» no es válido", data_center.trim()))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    browse_with(&client, &ep, creds, at).await
}

pub async fn browse_with(client: &reqwest::Client, ep: &Endpoints, creds: &Creds, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    use super::browse::{unique_names, wipe_json, Entry, Listing, MAX_ENTRIES};
    if let Some(item) = at.item {
        let mut secret = find_secret(client, ep, creds, item).await?;
        let mut names: Vec<String> = fields_of(&secret).into_iter().map(|(mut k, mut v)| {
            wipe(&mut v);
            std::mem::take(&mut k)
        }).collect();
        wipe_json(&mut secret);
        names.dedup();
        return Ok(Listing::fields(names));
    }
    const PAGE: usize = 100;
    let mut all: Vec<Value> = Vec::new();
    for page in 0.. {
        let json = api(client, ep, creds, &format!("/api/rest/json/v1/secrets?isAsc=true&pageNum={page}&rowPerPage={PAGE}")).await?;
        let list: Vec<Value> = json.pointer("/operation/Details").and_then(Value::as_array).cloned().unwrap_or_default();
        let n = list.len();
        // Names and IDs only; the rest (ciphertext) isn't kept.
        all.extend(list.iter().map(|s| serde_json::json!({ "secretname": name_of(s), "secretid": id_of(s) })));
        if n < PAGE || all.len() > MAX_ENTRIES {
            break;
        }
    }
    let unique = unique_names(all.iter().map(name_of));
    let mut out: Vec<Entry> = all
        .iter()
        .map(|s| {
            let (name, id) = (name_of(s).to_string(), id_of(s));
            let by_name = !name.trim().is_empty() && !name.chars().all(|c| c.is_ascii_digit()) && unique.contains(&name);
            Entry::item(if by_name { name.clone() } else { id.clone() }, if name.is_empty() { id.clone() } else { name }, "", id)
        })
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(Listing::complete(out))
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
    let mut data = base64::engine::general_purpose::STANDARD.decode(b64.trim().as_bytes()).map_err(|_| ())?;
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
        assert_eq!(pbkdf2_key(s(&v, "passphrase"), s(&v, "salt"), v["iterations"].as_u64().unwrap() as u32), s(&v, "master"));
        let details = json!({ "LOGIN": "PBKDF2_AES", "SALT": s(&v, "salt"), "ITERATION": v["iterations"], "PASSPHRASE": s(&v, "passphrase_check") });
        assert_eq!(master_key(&details, s(&v, "passphrase")).unwrap(), s(&v, "master"));
        assert_eq!(master_key(&details, "dummy-passphrase-n").unwrap_err(), WRONG_PASSPHRASE);
        // The iteration count may come as text.
        let as_text = json!({ "SALT": s(&v, "salt"), "ITERATION": "1000", "PASSPHRASE": s(&v, "passphrase_check") });
        assert_eq!(master_key(&as_text, s(&v, "passphrase")).unwrap(), s(&v, "master"));
        // Older accounts: SHA-256.
        let legacy = json!({ "LOGIN": "SHA256", "SALT": s(&v, "salt"), "PASSPHRASE": s(&v, "legacy_passauth") });
        assert_eq!(master_key(&legacy, s(&v, "legacy_passphrase")).unwrap(), s(&v, "legacy_master"));
        assert_eq!(master_key(&legacy, "other").unwrap_err(), WRONG_PASSPHRASE);
        assert!(master_key(&details, "").is_err());
    }

    #[test]
    fn aes_ctr_vectors() {
        let v = vectors();
        for a in v["aes"].as_array().unwrap() {
            let (key, plain, cipher) = (s(a, "key"), s(a, "plain"), s(a, "cipher"));
            assert_eq!(String::from_utf8(decrypt(cipher, key).unwrap()).unwrap(), plain, "{plain}");
            // Encrypting with the same nonce gives the same bytes.
            let raw = base64::engine::general_purpose::STANDARD.decode(cipher).unwrap();
            assert_eq!(encrypt(plain, key, raw[..8].try_into().unwrap()), cipher, "{plain}");
        }
        // Another key gives other bytes (and, here, not even UTF-8 text).
        let a = &v["aes"][2];
        assert_ne!(decrypt(s(a, "cipher"), "0000000000000000000000000000000000000000000000000000000000000000").unwrap(), s(a, "plain").as_bytes());
        assert!(decrypt("not base64!", "k").is_err());
        assert!(decrypt("AAAA", "k").is_err(), "shorter than the nonce");
    }

    #[test]
    fn organization_key_vectors() {
        let v = vectors();
        assert_eq!(decrypt_org_key(s(&v, "private_key"), s(&v, "shared_key_oaep"), s(&v, "master")).unwrap(), s(&v, "org_key"));
        assert_eq!(decrypt_org_key(s(&v, "private_key"), s(&v, "shared_key_pkcs1"), s(&v, "master")).unwrap(), s(&v, "org_key"));
        assert!(decrypt_org_key(s(&v, "private_key"), s(&v, "shared_key_oaep"), s(&v, "legacy_master")).is_err(), "another master key");
    }

    fn secret(name: &str, id: &str, shared: bool, data: &str) -> Value {
        json!({ "secretid": id, "secretname": name, "isshared": if shared { "YES" } else { "NO" }, "secretData": data })
    }

    #[test]
    fn fields_and_usernames() {
        let v = vectors();
        let personal = secret("pg-main", "1", false, s(&v, "personal_secret_data"));
        let r = |attr: &'static str, username: bool| Request { data_center: "eu", secret: "pg-main", attribute: attr, username };
        assert_eq!(values(&personal, s(&v, "master"), &r("", true)).unwrap(), ("pg-secret-1".to_string(), Some("app_user".to_string())));
        assert_eq!(values(&personal, s(&v, "master"), &r("username", false)).unwrap().0, "app_user");
        assert_eq!(values(&personal, s(&v, "master"), &r("PASSWORD", false)).unwrap().0, "pg-secret-1", "case doesn't matter");
        let e = values(&personal, s(&v, "master"), &r("pin", false)).unwrap_err();
        assert!(e.contains("no tiene el campo «pin»") && e.contains("password") && e.contains("username") && !e.contains("pg-secret"), "{e}");
        let e = values(&personal, s(&v, "legacy_master"), &r("", false)).unwrap_err();
        assert!(e.contains("no se pudo descifrar"), "a wrong key never shows garbage: {e}");
        let mut shared = secret("org-db", "2", true, s(&v, "shared_secret_data"));
        shared["customcolumn"] = json!(s(&v, "shared_custom_column"));
        assert_eq!(values(&shared, s(&v, "org_key"), &r("Port", false)).unwrap().0, "5432", "a custom field");
        assert_eq!(values(&shared, s(&v, "org_key"), &r("", true)).unwrap(), ("org-secret-9".to_string(), Some("org_user".to_string())));
    }

    #[test]
    fn names_and_ambiguity() {
        let list = vec![secret("pg-main", "11", false, "{}"), secret("pg-main-old", "12", false, "{}"), secret("PG-MAIN", "13", false, "{}")];
        assert_eq!(id_of(&pick_by_name(list.clone(), "pg-main").unwrap()), "11", "exact first");
        assert_eq!(id_of(&pick_by_name(list.clone(), "pg-main-OLD").unwrap()), "12", "else ignoring case");
        let twice = vec![secret("db", "21", false, "{}"), secret("db", "22", true, "{}")];
        let e = pick_by_name(twice, "db").unwrap_err();
        assert!(e.contains("2 secretos «db»") && e.contains("21") && e.contains("22"), "{e}");
        let e = pick_by_name(list, "mysql").unwrap_err();
        assert!(e.contains("no hay un secreto «mysql»") && e.contains("pg-main"), "{e}");
        assert!(pick_by_name(vec![], "x").unwrap_err().contains("no tiene acceso"));
    }

    #[test]
    fn errors() {
        assert!(oauth_error(200, "invalid_client", "").contains("ID o el secreto del cliente"));
        assert!(oauth_error(200, "invalid_code", "").contains("token de actualización"));
        assert!(oauth_error(400, "Access Denied", "You have made too many requests continuously.").contains("tan seguido"));
        assert_eq!(oauth_error(500, "", ""), "Zoho respondió 500 al pedir el token de acceso");
        assert!(api_error(200, "INVALID_OAUTHSCOPE", "").contains("ZohoVault.secrets.READ"));
        assert_eq!(api_error(500, "", ""), "Zoho Vault respondió 500");
        assert_eq!(api_error(200, "SOMETHING", "went wrong"), "Zoho Vault respondió SOMETHING: went wrong");
    }

    #[test]
    fn only_the_data_centres_hosts() {
        let ep = Endpoints::of("eu").unwrap();
        assert_eq!((ep.accounts.as_str(), ep.vault.as_str()), ("https://accounts.zoho.eu", "https://vault.zoho.eu"));
        let ep = Endpoints::of("ca").unwrap();
        assert_eq!(ep.vault, "https://vault.zohocloud.ca");
        for dc in ["", "evil.com", "https://evil.com", "eu.evil.com", "127.0.0.1"] {
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
    }

    fn authorized(h: &HeaderMap, m: &Mock) -> bool {
        let ok = h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Zoho-oauthtoken at-1");
        if !ok {
            *m.bad_auth.lock().unwrap() += 1;
        }
        ok
    }

    fn failed(code: &str) -> Json<Value> {
        Json(json!({ "operation": { "result": { "status": "failed", "error_code": code, "message": code } } }))
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
                        let name = q.get("passwordName").cloned().unwrap_or_default();
                        let mut shared = secret("org-db", "2000000000002", true, s(&v, "shared_secret_data"));
                        shared["customcolumn"] = json!(s(&v, "shared_custom_column"));
                        let all = vec![
                            secret("pg-main", "2000000000001", false, s(&v, "personal_secret_data")),
                            shared,
                            secret("twin", "2000000000003", false, "{}"),
                            secret("twin", "2000000000004", false, "{}"),
                        ];
                        let hits: Vec<Value> = all.into_iter().filter(|s| name_of(s).to_lowercase().contains(&name.to_lowercase())).collect();
                        Json(json!({ "operation": { "result": { "status": "Success", "message": "" }, "name": "GET_SECRETS", "Details": hits } }))
                    }
                }),
            )
            .route(
                "/api/rest/json/v1/secrets/{id}",
                aget(move |State(m): State<Arc<Mock>>, h: HeaderMap, AxPath(id): AxPath<String>| {
                    let v = v3.clone();
                    async move {
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
                aget(|State(m): State<Arc<Mock>>, h: HeaderMap| async move {
                    if !authorized(&h, &m) {
                        return failed("INVALID_OAUTHTOKEN");
                    }
                    let v = vectors();
                    Json(json!({ "operation": { "result": { "status": "Success", "message": "" }, "Details": {
                        "privateKey": v["private_key"], "sharedKey": v["shared_key_oaep"], "rsaKeyType": "RSA_4096"
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
        Creds { client_id: "1000.CLIENT".into(), client_secret: "dummy-client-secret".into(), refresh_token: refresh.into(), passphrase: passphrase.into() }
    }

    /// Everything against a mock of Zoho's endpoints (no Zoho account was
    /// available): token refresh, GET_LOGIN, by name and by ID, personal and
    /// shared secrets, the organization key, error mapping, token reuse.
    #[tokio::test]
    async fn against_a_mock_of_the_api() {
        let (base, mock) = mock_server().await;
        let ep = Endpoints { accounts: base.clone(), vault: base.clone() };
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let v = vectors();
        let pass = s(&v, "passphrase").to_string();
        let r = |secret: &'static str, attribute: &'static str, username: bool| Request { data_center: "eu", secret, attribute, username };
        // Each test uses its own refresh token: the access token cache is
        // keyed by it.
        let c = creds(&pass, "1000.dummy-refresh");
        forget_token(&token_cache_key(&ep, &c));
        assert_eq!(read_with(&client, &ep, &r("pg-main", "", true), &c).await.unwrap(), ("pg-secret-1".into(), Some("app_user".into())));
        assert_eq!(read_with(&client, &ep, &r("2000000000001", "username", false), &c).await.unwrap().0, "app_user", "by ID");
        assert_eq!(read_with(&client, &ep, &r("org-db", "", true), &c).await.unwrap(), ("org-secret-9".into(), Some("org_user".into())), "shared: the organization key");
        assert_eq!(read_with(&client, &ep, &r("org-db", "Port", false), &c).await.unwrap().0, "5432");
        assert_eq!(*mock.token_requests.lock().unwrap(), 1, "one access token for all of them");
        // Zoho says the access token expired: a new one, once.
        *mock.expire_once.lock().unwrap() = true;
        assert_eq!(read_with(&client, &ep, &r("pg-main", "", false), &c).await.unwrap().0, "pg-secret-1");
        assert_eq!(*mock.token_requests.lock().unwrap(), 2);
        let e = read_with(&client, &ep, &r("twin", "", false), &c).await.unwrap_err();
        assert!(e.contains("2 secretos «twin»") && e.contains("2000000000003"), "{e}");
        let e = read_with(&client, &ep, &r("nothing", "", false), &c).await.unwrap_err();
        assert!(e.contains("no hay un secreto «nothing»"), "{e}");
        let e = read_with(&client, &ep, &r("2000000000999", "", false), &c).await.unwrap_err();
        assert!(e.contains("SECRET_NOT_FOUND"), "{e}");
        let e = read_with(&client, &ep, &r("pg-main", "", false), &creds("wrong passphrase", "1000.dummy-refresh")).await.unwrap_err();
        assert_eq!(e, WRONG_PASSPHRASE);
        // A wrong refresh token or client secret: Zoho's answer, never the values.
        let bad = creds(&pass, "1000.revoked");
        let e = read_with(&client, &ep, &r("pg-main", "", false), &bad).await.unwrap_err();
        assert!(e.contains("token de actualización") && !e.contains("revoked"), "{e}");
        let mut bad = creds(&pass, "1000.dummy-refresh-2");
        bad.client_secret = "nope".into();
        let e = read_with(&client, &ep, &r("pg-main", "", false), &bad).await.unwrap_err();
        assert!(e.contains("secreto del cliente") && !e.contains("nope"), "{e}");
        assert_eq!(*mock.bad_auth.lock().unwrap(), 0, "every API call carried the access token");
        forget_token(&token_cache_key(&ep, &c));
    }

    /// Browsing: names (a repeated one by its ID), then a secret's field
    /// names, never a value or its ciphertext.
    #[tokio::test]
    async fn browse_against_the_mock() {
        let (base, _) = mock_server().await;
        let ep = Endpoints { accounts: base.clone(), vault: base.clone() };
        let client = reqwest::Client::new();
        let c = creds("", "1000.dummy-refresh");
        let l = browse_with(&client, &ep, &c, Default::default()).await.unwrap();
        let ids: Vec<&str> = l.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["org-db", "pg-main", "2000000000003", "2000000000004"]);
        let at = crate::secret_sources::browse::At { item: Some("pg-main"), ..Default::default() };
        let l = browse_with(&client, &ep, &c, at).await.unwrap();
        let fields = l.fields.clone().unwrap();
        assert!(fields.contains(&"password".to_string()) && fields.contains(&"username".to_string()), "{fields:?}");
        let v = vectors();
        let json = serde_json::to_string(&l).unwrap();
        assert!(!json.contains(s(&v, "personal_secret_data")), "{json}");
        forget_token(&token_cache_key(&ep, &c));
    }

    /// The real endpoints refuse plain HTTP: `https_only`.
    #[tokio::test]
    async fn production_client_is_https_only() {
        let (base, _) = mock_server().await;
        let client = reqwest::Client::builder().https_only(true).build().unwrap();
        let ep = Endpoints { accounts: base.clone(), vault: base };
        let c = creds("x", "1000.dummy-refresh-https");
        let e = read_with(&client, &ep, &Request { data_center: "eu", secret: "pg-main", attribute: "", username: false }, &c).await.unwrap_err();
        assert!(e.starts_with("no se pudo conectar con Zoho"), "{e}");
    }
}
